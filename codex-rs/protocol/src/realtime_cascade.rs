//! Cascade voice mode for thread realtime.
//!
//! A cascade session needs no realtime upstream: input speech is cut into utterances locally,
//! transcribed by the model provider's `audio/transcriptions` endpoint and submitted as a normal
//! user turn; the turn's assistant text is spoken sentence by sentence through the provider's
//! `audio/speech` endpoint and streamed back as realtime output audio.

/// Options of a cascade session (`thread/realtime/start` with the cascade transport).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CascadeStartParams {
    /// Voice id from the provider's voice list; `None` lets the provider pick its default.
    pub voice: Option<String>,
    /// Language hint for transcription (for example `en` or `zh`); `None` lets the provider detect it.
    pub language: Option<String>,
    /// Client-chosen id echoed in the started notification.
    pub realtime_session_id: Option<String>,
    /// False when the client asked for text output: transcripts only, no speech requests.
    pub speak: bool,
}

/// One voice the provider's speech endpoint offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CascadeVoice {
    pub id: String,
    pub label: Option<String>,
    pub lang: Option<String>,
}

/// The provider's voice list for cascade sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CascadeVoicesList {
    pub voices: Vec<CascadeVoice>,
    pub default_voice: Option<String>,
}
