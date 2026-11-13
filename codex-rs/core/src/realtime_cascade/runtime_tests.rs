use super::*;
use crate::realtime_cascade::endpoints::ProviderAudioEndpoints;
use crate::realtime_cascade::endpoints::tests::test_endpoints;
use pretty_assertions::assert_eq;
use std::time::Instant;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::mpsc::unbounded_channel;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_partial_json;
use wiremock::matchers::method;
use wiremock::matchers::path;

const INPUT_RATE: u32 = 24_000;
const WAIT: Duration = Duration::from_secs(10);
/// Speech answers are 200 ms at 16 kHz: 4800 samples at the output rate, two 100 ms chunks.
const SPEECH_SAMPLES: usize = 3_200;

struct FakeHost {
    events: UnboundedSender<RealtimeEvent>,
    submitted: UnboundedSender<String>,
}

impl CascadeHost for FakeHost {
    async fn emit(&self, event: RealtimeEvent) {
        let _ = self.events.send(event);
    }

    async fn submit_user_text(&self, text: String) -> Result<(), String> {
        let _ = self.submitted.send(text);
        Ok(())
    }
}

struct Harness {
    server: MockServer,
    runtime: CascadeRuntime,
    events: UnboundedReceiver<RealtimeEvent>,
    submitted: UnboundedReceiver<String>,
}

async fn harness(server: MockServer, speak: bool) -> Harness {
    let endpoints: ProviderAudioEndpoints<codex_api::ReqwestTransport> =
        test_endpoints(&format!("{}/v1", server.uri()));
    let (events_tx, events) = unbounded_channel();
    let (submitted_tx, submitted) = unbounded_channel();
    let runtime = CascadeRuntime::spawn(
        Arc::new(endpoints),
        Arc::new(FakeHost {
            events: events_tx,
            submitted: submitted_tx,
        }),
        CascadeOptions {
            voice: Some("af_heart".to_string()),
            language: Some("en".to_string()),
            speak,
        },
    );
    Harness {
        server,
        runtime,
        events,
        submitted,
    }
}

fn speech_wav() -> Vec<u8> {
    wav::encode_pcm16_mono(16_000, &vec![1_000; SPEECH_SAMPLES])
}

fn frame(samples: &[i16]) -> RealtimeAudioFrame {
    RealtimeAudioFrame {
        data: wav::pcm16_to_base64(samples),
        sample_rate: INPUT_RATE,
        num_channels: 1,
        samples_per_channel: Some(samples.len() as u32),
        item_id: None,
    }
}

/// `ms` of a loud square wave (voiced) or of silence, in 100 ms chunks.
fn push(runtime: &CascadeRuntime, ms: u32, loud: bool) {
    let chunk = (INPUT_RATE / 10) as usize;
    let amplitude: i16 = if loud { 8_000 } else { 0 };
    for _ in 0..ms / 100 {
        let samples: Vec<i16> = (0..chunk)
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect();
        runtime.push_audio(&frame(&samples)).unwrap();
    }
}

fn speak_utterance(runtime: &CascadeRuntime) {
    push(runtime, 500, true);
    push(runtime, 800, false);
}

async fn next_event(
    events: &mut UnboundedReceiver<RealtimeEvent>,
    mut wanted: impl FnMut(&RealtimeEvent) -> bool,
    seen: &mut Vec<RealtimeEvent>,
) -> RealtimeEvent {
    tokio::time::timeout(WAIT, async {
        loop {
            let event = events.recv().await.expect("host dropped");
            seen.push(event.clone());
            if wanted(&event) {
                return event;
            }
        }
    })
    .await
    .expect("event did not arrive")
}

async fn speech_inputs(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|request| request.url.path() == "/v1/audio/speech")
        .map(|request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            body["input"].as_str().unwrap_or_default().to_string()
        })
        .collect()
}

async fn wait_for_requests(server: &MockServer, request_path: &str, count: usize) {
    tokio::time::timeout(WAIT, async {
        loop {
            let seen = server
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .filter(|request| request.url.path() == request_path)
                .count();
            if seen >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("request did not arrive");
}

fn audio_samples(events: &[RealtimeEvent]) -> Vec<(Option<String>, u32)> {
    events
        .iter()
        .filter_map(|event| match event {
            RealtimeEvent::AudioOut(frame) => {
                assert_eq!(frame.sample_rate, wav::OUTPUT_SAMPLE_RATE);
                assert_eq!(frame.num_channels, 1);
                let samples = wav::pcm16_from_base64(&frame.data).unwrap().len() as u32;
                assert_eq!(frame.samples_per_channel, Some(samples));
                Some((frame.item_id.clone(), samples))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn an_utterance_becomes_a_turn_and_the_answer_is_spoken() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": " hello there "})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(speech_wav()))
        .expect(2)
        .mount(&server)
        .await;
    let mut h = harness(server, true).await;
    let mut seen = Vec::new();

    speak_utterance(&h.runtime);
    next_event(
        &mut h.events,
        |e| matches!(e, RealtimeEvent::InputAudioSpeechStarted(_)),
        &mut seen,
    )
    .await;
    let done = next_event(
        &mut h.events,
        |e| matches!(e, RealtimeEvent::InputTranscriptDone(_)),
        &mut seen,
    )
    .await;
    assert_eq!(
        done,
        RealtimeEvent::InputTranscriptDone(RealtimeTranscriptDone {
            text: "hello there".to_string()
        })
    );
    let submitted = tokio::time::timeout(WAIT, h.submitted.recv())
        .await
        .unwrap();
    assert_eq!(submitted.as_deref(), Some("hello there"));

    // The transcription request: a WAV of the utterance, the language and the provider auth.
    let requests = h.server.received_requests().await.unwrap();
    let transcription = requests
        .iter()
        .find(|r| r.url.path() == "/v1/audio/transcriptions")
        .unwrap();
    assert_eq!(
        transcription.headers.get("authorization").unwrap(),
        "Bearer test-token"
    );
    let body = String::from_utf8_lossy(&transcription.body);
    assert!(body.contains("name=\"language\"\r\n\r\nen\r\n"));
    assert!(body.contains("Content-Type: audio/wav\r\n\r\nRIFF"));

    // The answer streams in; each sentence is spoken in order.
    h.runtime.message_started("m1", "").unwrap();
    h.runtime
        .message_delta("m1", "First sentence. Sec")
        .unwrap();
    h.runtime.message_delta("m1", "ond").unwrap();
    h.runtime
        .message_completed("m1", "First sentence. Second")
        .unwrap();
    let done = next_event(
        &mut h.events,
        |e| matches!(e, RealtimeEvent::OutputTranscriptDone(_)),
        &mut seen,
    )
    .await;
    assert_eq!(
        done,
        RealtimeEvent::OutputTranscriptDone(RealtimeTranscriptDone {
            text: "First sentence. Second".to_string()
        })
    );
    assert_eq!(
        speech_inputs(&h.server).await,
        vec!["First sentence.".to_string(), "Second".to_string()]
    );
    let speech: serde_json::Value = serde_json::from_slice(
        &h.server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .find(|r| r.url.path() == "/v1/audio/speech")
            .unwrap()
            .body,
    )
    .unwrap();
    assert_eq!(speech["voice"], "af_heart");
    assert_eq!(speech["response_format"], "wav");
    // 2 sentences × 4800 output samples, in 100 ms (2400-sample) chunks, tagged with the message.
    assert_eq!(
        audio_samples(&seen),
        vec![(Some("m1".to_string()), 2_400); 4]
    );
    let deltas: Vec<_> = seen
        .iter()
        .filter_map(|e| match e {
            RealtimeEvent::OutputTranscriptDelta(delta) => Some(delta.delta.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["First sentence.", "Second"]);
    h.runtime.stop().await;
}

#[tokio::test]
async fn speech_onset_cancels_queued_and_playing_speech() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .and(body_partial_json(serde_json::json!({"input": "Old one."})))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(speech_wav())
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(speech_wav()))
        .mount(&server)
        .await;
    let mut h = harness(server, true).await;
    let mut seen = Vec::new();

    h.runtime.message_started("m1", "").unwrap();
    h.runtime.message_delta("m1", "Old one. Old two. ").unwrap();
    wait_for_requests(&h.server, "/v1/audio/speech", 1).await;

    // The user starts talking over the answer.
    push(&h.runtime, 100, true);
    next_event(
        &mut h.events,
        |e| matches!(e, RealtimeEvent::InputAudioSpeechStarted(_)),
        &mut seen,
    )
    .await;
    next_event(
        &mut h.events,
        |e| matches!(e, RealtimeEvent::ResponseCancelled(_)),
        &mut seen,
    )
    .await;

    // The interrupted message keeps streaming but stays muted; a new message is spoken.
    h.runtime.message_delta("m1", "Old three. ").unwrap();
    h.runtime.message_completed("m1", "").unwrap();
    h.runtime.message_started("m2", "Fresh answer.").unwrap();
    h.runtime.message_completed("m2", "Fresh answer.").unwrap();
    let done = next_event(
        &mut h.events,
        |e| matches!(e, RealtimeEvent::OutputTranscriptDone(_)),
        &mut seen,
    )
    .await;
    assert_eq!(
        done,
        RealtimeEvent::OutputTranscriptDone(RealtimeTranscriptDone {
            text: "Fresh answer.".to_string()
        })
    );
    assert_eq!(
        speech_inputs(&h.server).await,
        vec!["Old one.".to_string(), "Fresh answer.".to_string()]
    );
    assert_eq!(
        audio_samples(&seen),
        vec![(Some("m2".to_string()), 2_400); 2]
    );
    h.runtime.stop().await;
}

#[tokio::test]
async fn failures_are_reported_and_the_loop_goes_on() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({
            "error": {"type": "unavailable", "code": "voice_unavailable", "message": "no tier"}
        })))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"text": "again"})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"not a wav".to_vec()))
        .mount(&server)
        .await;
    let mut h = harness(server, true).await;
    let mut seen = Vec::new();

    speak_utterance(&h.runtime);
    let error = next_event(
        &mut h.events,
        |e| matches!(e, RealtimeEvent::Error(_)),
        &mut seen,
    )
    .await;
    assert_eq!(
        error,
        RealtimeEvent::Error("transcription failed: HTTP 503 (voice_unavailable)".to_string())
    );
    speak_utterance(&h.runtime);
    let submitted = tokio::time::timeout(WAIT, h.submitted.recv())
        .await
        .unwrap();
    assert_eq!(submitted.as_deref(), Some("again"));

    h.runtime.push_speech("Say this.").unwrap();
    let error = next_event(
        &mut h.events,
        |e| matches!(e, RealtimeEvent::Error(_)),
        &mut seen,
    )
    .await;
    assert_eq!(
        error,
        RealtimeEvent::Error("speech failed: the speech response is not a WAV file".to_string())
    );
    // A sentence that was not played never shows up as spoken.
    tokio::time::sleep(Duration::from_millis(200)).await;
    while let Ok(event) = h.events.try_recv() {
        seen.push(event);
    }
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, RealtimeEvent::OutputTranscriptDone(_)))
    );
    assert!(audio_samples(&seen).is_empty());
    h.runtime.stop().await;
}

#[tokio::test]
async fn stop_returns_promptly_while_transcription_is_pending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"text": "late"}))
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&server)
        .await;
    let mut h = harness(server, true).await;
    speak_utterance(&h.runtime);
    wait_for_requests(&h.server, "/v1/audio/transcriptions", 1).await;

    let started = Instant::now();
    h.runtime.stop().await;
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(1), "stop took {elapsed:?}");
    // Nothing was submitted after the stop.
    assert!(h.submitted.try_recv().is_err());
}

#[tokio::test]
async fn stop_returns_promptly_while_speech_is_pending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(speech_wav())
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&server)
        .await;
    let h = harness(server, true).await;
    h.runtime.push_speech("One. Two. Three.").unwrap();
    wait_for_requests(&h.server, "/v1/audio/speech", 1).await;
    let started = Instant::now();
    h.runtime.stop().await;
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn queues_are_bounded() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"text": "slow"}))
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(speech_wav())
                .set_delay(Duration::from_secs(30)),
        )
        .mount(&server)
        .await;
    let h = harness(server, true).await;
    speak_utterance(&h.runtime);
    wait_for_requests(&h.server, "/v1/audio/transcriptions", 1).await;
    // The transcription task is busy: the utterance queue takes UTTERANCE_QUEUE more.
    for i in 0..UTTERANCE_QUEUE {
        h.runtime.push_text(format!("text {i}")).unwrap();
    }
    assert_eq!(
        h.runtime.push_text("one too many".to_string()),
        Err("voice session is busy; try again".to_string())
    );

    // The speech task is busy with the first sentence: the queue holds SPEECH_QUEUE jobs.
    h.runtime.push_speech("Start.").unwrap();
    wait_for_requests(&h.server, "/v1/audio/speech", 1).await;
    let many = "Word. ".repeat(SPEECH_QUEUE + 5);
    assert_eq!(
        h.runtime.push_speech(&many),
        Err("speech queue is full; skipping".to_string())
    );
    h.runtime.stop().await;
}

#[tokio::test]
async fn text_output_sends_no_speech_requests() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(speech_wav()))
        .expect(0)
        .mount(&server)
        .await;
    let h = harness(server, false).await;
    h.runtime.message_started("m1", "").unwrap();
    h.runtime.message_completed("m1", "Spoken? No.").unwrap();
    h.runtime.push_speech("Nor this.").unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(speech_inputs(&h.server).await.is_empty());
    h.runtime.stop().await;
}

#[tokio::test]
async fn invalid_audio_is_refused_with_a_reason() {
    let server = MockServer::start().await;
    let h = harness(server, true).await;
    let mut bad_rate = frame(&[0; 10]);
    bad_rate.sample_rate = 96_000;
    let mut bad_channels = frame(&[0; 10]);
    bad_channels.num_channels = 3;
    let mut bad_data = frame(&[0; 10]);
    bad_data.data = "%%%".to_string();
    let mut too_large = frame(&[0; 10]);
    too_large.data = "A".repeat(MAX_AUDIO_CHUNK_BASE64_BYTES + 4);
    let cases = [
        (bad_rate, "audio sample rate must be 8000..=48000 Hz"),
        (bad_channels, "audio must be mono or stereo"),
        (bad_data, "audio data is not valid base64"),
        (too_large, "audio chunk is too large"),
    ];
    for (frame, expected) in cases {
        assert_eq!(h.runtime.push_audio(&frame), Err(expected.to_string()));
    }
    // Stereo is accepted (and downmixed).
    let mut stereo = frame(&[0; 10]);
    stereo.num_channels = 2;
    assert_eq!(h.runtime.push_audio(&stereo), Ok(()));
    h.runtime.stop().await;
}

#[tokio::test]
async fn stop_ends_every_task() {
    let server = MockServer::start().await;
    let h = harness(server, true).await;
    let runtime = h.runtime;
    let shared = Arc::clone(&runtime.shared);
    runtime.stop().await;
    assert!(shared.stop.is_cancelled());
    assert!(shared.barge_token().is_cancelled());
}
