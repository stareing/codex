//! The model provider's audio endpoints, on the same base URL and auth as its Responses API:
//!
//! | Path | Request | Answer |
//! |---|---|---|
//! | `POST audio/transcriptions` | multipart `file` (WAV), `language?`, `response_format=json` | `{text}` |
//! | `POST audio/speech` | JSON `{input, voice?, response_format: "wav"}` | WAV bytes |
//! | `GET audio/voices` | — | `{voices: [{id, label?, lang?}], default_voice?}` |
//!
//! Every request has a timeout and a response size limit and is sent once (no retries: a slow
//! voice answer is worse than an error the user can repeat).

use std::future::Future;
use std::time::Duration;

use codex_api::Provider as ApiProvider;
use codex_api::SharedAuthProvider;
use codex_client::HttpTransport;
use codex_client::Request;
use codex_client::TransportError;
use codex_protocol::realtime_cascade::CascadeVoice;
use codex_protocol::realtime_cascade::CascadeVoicesList;
use http::HeaderValue;
use http::Method;
use http::header::ACCEPT;
use http::header::CONTENT_TYPE;
use serde::Deserialize;
use serde_json::json;

pub(crate) const TRANSCRIPTIONS_PATH: &str = "audio/transcriptions";
pub(crate) const SPEECH_PATH: &str = "audio/speech";
pub(crate) const VOICES_PATH: &str = "audio/voices";

/// Per-request deadlines (the transport's timeout; callers add their own cancellation).
pub(crate) const TRANSCRIPTION_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const SPEECH_TIMEOUT: Duration = Duration::from_secs(45);
pub(crate) const VOICES_TIMEOUT: Duration = Duration::from_secs(10);

/// Response size limits.
const MAX_TRANSCRIPTION_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_SPEECH_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_VOICES_RESPONSE_BYTES: usize = 512 * 1024;
/// At most this many voices are kept from a list.
const MAX_VOICES: usize = 512;
/// Error codes copied from a failed response are cut to this length.
const MAX_ERROR_CODE_CHARS: usize = 64;

/// A failed audio request, worded for the user (no URLs, keys or response bodies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EndpointError(pub(crate) String);

impl std::fmt::Display for EndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub(crate) trait AudioEndpoints: Send + Sync + 'static {
    fn transcribe(
        &self,
        wav: Vec<u8>,
        language: Option<String>,
    ) -> impl Future<Output = Result<String, EndpointError>> + Send;

    fn speak(
        &self,
        text: String,
        voice: Option<String>,
    ) -> impl Future<Output = Result<Vec<u8>, EndpointError>> + Send;

    fn voices(&self) -> impl Future<Output = Result<CascadeVoicesList, EndpointError>> + Send;
}

/// [`AudioEndpoints`] over the provider's HTTP API.
pub(crate) struct ProviderAudioEndpoints<T: HttpTransport> {
    transport: T,
    provider: ApiProvider,
    auth: SharedAuthProvider,
}

impl<T: HttpTransport + 'static> ProviderAudioEndpoints<T> {
    pub(crate) fn new(transport: T, provider: ApiProvider, auth: SharedAuthProvider) -> Self {
        Self {
            transport,
            provider,
            auth,
        }
    }

    async fn send(
        &self,
        mut request: Request,
        what: &str,
    ) -> Result<codex_client::Response, EndpointError> {
        request = self.auth.apply_auth(request).await.map_err(|_| {
            EndpointError(format!(
                "{what} failed: the provider credentials are not available"
            ))
        })?;
        self.transport
            .execute(request)
            .await
            .map_err(|error| describe_transport_error(what, &error))
    }

    fn request(&self, method: Method, path: &str, timeout: Duration, limit: usize) -> Request {
        let mut request = self.provider.build_request(method, path);
        request.timeout = Some(timeout);
        request.response_body_limit_bytes = Some(limit);
        request
    }
}

fn describe_transport_error(what: &str, error: &TransportError) -> EndpointError {
    let detail = match error {
        TransportError::Http { status, body, .. } => match body.as_deref().and_then(error_code) {
            Some(code) => format!("HTTP {} ({code})", status.as_u16()),
            None => format!("HTTP {}", status.as_u16()),
        },
        TransportError::Timeout => "timed out".to_string(),
        TransportError::ResponseTooLarge { .. } => "the response is too large".to_string(),
        TransportError::Policy(_) => "blocked by the network policy".to_string(),
        TransportError::RetryLimit
        | TransportError::Connection(_)
        | TransportError::Network(_)
        | TransportError::Build(_) => "the provider could not be reached".to_string(),
    };
    EndpointError(format!("{what} failed: {detail}"))
}

/// `error.code` (the OpenAI error shape) or a top-level `code`, if short and plain.
fn error_code(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let code = value
        .pointer("/error/code")
        .or_else(|| value.get("code"))?
        .as_str()?;
    let plain = !code.is_empty()
        && code.chars().count() <= MAX_ERROR_CODE_CHARS
        && code
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    plain.then(|| code.to_string())
}

/// A form boundary absent from the file.
fn boundary_for(file: &[u8]) -> String {
    loop {
        let boundary = format!("cascade-{}", uuid::Uuid::new_v4().simple());
        if !file
            .windows(boundary.len())
            .any(|window| window == boundary.as_bytes())
        {
            return boundary;
        }
    }
}

pub(crate) fn transcription_form(wav: &[u8], language: Option<&str>) -> (String, Vec<u8>) {
    let boundary = boundary_for(wav);
    let mut body = Vec::with_capacity(wav.len() + 512);
    let mut field = |name: &str, value: &str| {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    };
    field("response_format", "json");
    if let Some(language) = language {
        field("language", language);
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"speech.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(wav);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

#[derive(Deserialize)]
struct TranscriptionResponse {
    text: String,
}

#[derive(Deserialize)]
struct VoicesResponse {
    #[serde(default)]
    voices: Vec<VoiceEntry>,
    #[serde(default)]
    default_voice: Option<String>,
}

#[derive(Deserialize)]
struct VoiceEntry {
    id: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    lang: Option<String>,
}

impl<T: HttpTransport + 'static> AudioEndpoints for ProviderAudioEndpoints<T> {
    async fn transcribe(
        &self,
        wav: Vec<u8>,
        language: Option<String>,
    ) -> Result<String, EndpointError> {
        let what = "transcription";
        let (content_type, body) = transcription_form(&wav, language.as_deref());
        let mut request = self
            .request(
                Method::POST,
                TRANSCRIPTIONS_PATH,
                TRANSCRIPTION_TIMEOUT,
                MAX_TRANSCRIPTION_RESPONSE_BYTES,
            )
            .with_raw_body(body);
        let content_type = HeaderValue::from_str(&content_type)
            .map_err(|_| EndpointError(format!("{what} failed: bad form boundary")))?;
        request.headers.insert(CONTENT_TYPE, content_type);
        request
            .headers
            .insert(ACCEPT, HeaderValue::from_static("application/json"));
        let response = self.send(request, what).await?;
        serde_json::from_slice::<TranscriptionResponse>(&response.body)
            .map(|parsed| parsed.text)
            .map_err(|_| EndpointError(format!("{what} failed: unexpected response")))
    }

    async fn speak(&self, text: String, voice: Option<String>) -> Result<Vec<u8>, EndpointError> {
        let what = "speech";
        let mut payload = json!({"input": text, "response_format": "wav"});
        if let Some(voice) = voice {
            payload["voice"] = json!(voice);
        }
        let mut request = self
            .request(
                Method::POST,
                SPEECH_PATH,
                SPEECH_TIMEOUT,
                MAX_SPEECH_RESPONSE_BYTES,
            )
            .with_json(&payload);
        request
            .headers
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        request
            .headers
            .insert(ACCEPT, HeaderValue::from_static("audio/wav"));
        Ok(self.send(request, what).await?.body.to_vec())
    }

    async fn voices(&self) -> Result<CascadeVoicesList, EndpointError> {
        let what = "voice list";
        let mut request = self.request(
            Method::GET,
            VOICES_PATH,
            VOICES_TIMEOUT,
            MAX_VOICES_RESPONSE_BYTES,
        );
        request
            .headers
            .insert(ACCEPT, HeaderValue::from_static("application/json"));
        let response = self.send(request, what).await?;
        let parsed: VoicesResponse = serde_json::from_slice(&response.body)
            .map_err(|_| EndpointError(format!("{what} failed: unexpected response")))?;
        Ok(CascadeVoicesList {
            voices: parsed
                .voices
                .into_iter()
                .take(MAX_VOICES)
                .map(|voice| CascadeVoice {
                    id: voice.id,
                    label: voice.label,
                    lang: voice.lang,
                })
                .collect(),
            default_voice: parsed.default_voice,
        })
    }
}

#[cfg(test)]
#[path = "endpoints_tests.rs"]
pub(crate) mod tests;
