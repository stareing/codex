//! The tasks of one cascade session and the handle that feeds them.
//!
//! ```text
//! appendAudio ─try_send─▶ [audio, AUDIO_QUEUE] ─▶ VAD task ─try_send─▶ [utterances, UTTERANCE_QUEUE]
//! appendText ──────────────────────────────────────────────try_send──▶        │
//!                                                                      transcription task
//!                                                     (STT ─▶ transcript ─▶ user turn on the thread)
//! assistant text ─▶ splitter ─try_send─▶ [speech, SPEECH_QUEUE] ─▶ speech task (TTS ─▶ outputAudio)
//! ```
//!
//! Every queue is bounded and fed with `try_send` (a full queue drops and reports, nothing blocks
//! the thread's event path); every HTTP call has a deadline and is raced against cancellation.
//! Barge-in (speech starts, or text arrives) bumps the speech generation: queued sentences of the
//! old generation are skipped, the in-flight request and frame stream are cancelled and the
//! messages being spoken are muted. Stop cancels all tasks; they end at their next await.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_protocol::protocol::RealtimeAudioFrame;
use codex_protocol::protocol::RealtimeEvent;
use codex_protocol::protocol::RealtimeInputAudioSpeechStarted;
use codex_protocol::protocol::RealtimeResponseCancelled;
use codex_protocol::protocol::RealtimeTranscriptDelta;
use codex_protocol::protocol::RealtimeTranscriptDone;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::endpoints::AudioEndpoints;
use super::endpoints::SPEECH_TIMEOUT;
use super::endpoints::TRANSCRIPTION_TIMEOUT;
use super::sentences::SentenceSplitter;
use super::vad::EnergyVad;
use super::vad::VadEvent;
use super::wav;

/// Input chunks waiting for the VAD (about 10 s of 100 ms chunks).
pub(crate) const AUDIO_QUEUE: usize = 128;
/// Finished utterances or texts waiting for transcription and submission.
pub(crate) const UTTERANCE_QUEUE: usize = 4;
/// Sentences waiting for speech.
pub(crate) const SPEECH_QUEUE: usize = 64;
/// Assistant messages tracked at once (a turn has few; stale ones are evicted).
pub(crate) const MAX_TRACKED_MESSAGES: usize = 16;
/// Largest accepted base64 audio chunk (about 10 s at 48 kHz stereo).
pub(crate) const MAX_AUDIO_CHUNK_BASE64_BYTES: usize = 2_600_000;
/// Longest accepted text input.
pub(crate) const MAX_TEXT_INPUT_CHARS: usize = 20_000;
/// Output audio is streamed in chunks of this length.
pub(crate) const OUTPUT_CHUNK_MS: u32 = 100;
/// How long stop waits for the tasks before aborting them.
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(2);

/// What the session gives the tasks: event delivery and turn submission.
pub(crate) trait CascadeHost: Send + Sync + 'static {
    fn emit(&self, event: RealtimeEvent) -> impl Future<Output = ()> + Send;
    fn submit_user_text(&self, text: String) -> impl Future<Output = Result<(), String>> + Send;
}

#[derive(Debug, Clone, Default)]
pub(crate) struct CascadeOptions {
    pub(crate) voice: Option<String>,
    pub(crate) language: Option<String>,
    pub(crate) speak: bool,
}

struct AudioChunk {
    sample_rate: u32,
    samples: Vec<i16>,
}

enum Utterance {
    Audio { sample_rate: u32, samples: Vec<i16> },
    Text(String),
}

enum SpeechJob {
    Sentence {
        generation: u64,
        item_id: Option<String>,
        text: String,
    },
    MessageDone {
        generation: u64,
        item_id: Option<String>,
    },
}

impl SpeechJob {
    fn generation(&self) -> u64 {
        match self {
            SpeechJob::Sentence { generation, .. } | SpeechJob::MessageDone { generation, .. } => {
                *generation
            }
        }
    }
}

struct TrackedMessage {
    splitter: SentenceSplitter,
    generation: u64,
    received_text: bool,
    order: u64,
}

struct Shared {
    stop: CancellationToken,
    options: CascadeOptions,
    generation: AtomicU64,
    /// Cancelled on barge-in and replaced; a child of `stop`.
    barge: StdMutex<CancellationToken>,
    /// Sentences queued or being spoken.
    pending_speech: AtomicUsize,
    messages: StdMutex<HashMap<String, TrackedMessage>>,
    next_order: AtomicU64,
}

impl Shared {
    fn barge_token(&self) -> CancellationToken {
        self.barge
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Cancels speech; returns whether any was queued or playing.
    fn barge_in(&self) -> bool {
        self.generation.fetch_add(1, Ordering::AcqRel);
        {
            let mut barge = self
                .barge
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            barge.cancel();
            *barge = self.stop.child_token();
        }
        // Messages still streaming keep the old generation: their later sentences are skipped.
        self.pending_speech.load(Ordering::Acquire) > 0
    }
}

/// The running session: what the session's handlers call into.
pub(crate) struct CascadeRuntime {
    shared: Arc<Shared>,
    audio_tx: mpsc::Sender<AudioChunk>,
    utterance_tx: mpsc::Sender<Utterance>,
    speech_tx: mpsc::Sender<SpeechJob>,
    tasks: Vec<JoinHandle<()>>,
}

impl CascadeRuntime {
    pub(crate) fn spawn<E: AudioEndpoints, H: CascadeHost>(
        endpoints: Arc<E>,
        host: Arc<H>,
        options: CascadeOptions,
    ) -> Self {
        let stop = CancellationToken::new();
        let shared = Arc::new(Shared {
            barge: StdMutex::new(stop.child_token()),
            stop,
            options,
            generation: AtomicU64::new(0),
            pending_speech: AtomicUsize::new(0),
            messages: StdMutex::new(HashMap::new()),
            next_order: AtomicU64::new(0),
        });
        let (audio_tx, audio_rx) = mpsc::channel(AUDIO_QUEUE);
        let (utterance_tx, utterance_rx) = mpsc::channel(UTTERANCE_QUEUE);
        let (speech_tx, speech_rx) = mpsc::channel(SPEECH_QUEUE);
        let tasks = vec![
            tokio::spawn(vad_task(
                Arc::clone(&shared),
                Arc::clone(&host),
                audio_rx,
                utterance_tx.clone(),
            )),
            tokio::spawn(transcription_task(
                Arc::clone(&shared),
                Arc::clone(&endpoints),
                Arc::clone(&host),
                utterance_rx,
            )),
            tokio::spawn(speech_task(Arc::clone(&shared), endpoints, host, speech_rx)),
        ];
        Self {
            shared,
            audio_tx,
            utterance_tx,
            speech_tx,
            tasks,
        }
    }

    /// Queues one input chunk (PCM16 LE, any rate the protocol allows, mono or stereo).
    pub(crate) fn push_audio(&self, frame: &RealtimeAudioFrame) -> Result<(), String> {
        if frame.data.len() > MAX_AUDIO_CHUNK_BASE64_BYTES {
            return Err("audio chunk is too large".to_string());
        }
        if !(wav::MIN_SAMPLE_RATE..=wav::MAX_SAMPLE_RATE).contains(&frame.sample_rate) {
            return Err(format!(
                "audio sample rate must be {}..={} Hz",
                wav::MIN_SAMPLE_RATE,
                wav::MAX_SAMPLE_RATE
            ));
        }
        if frame.num_channels == 0 || frame.num_channels > wav::MAX_CHANNELS {
            return Err("audio must be mono or stereo".to_string());
        }
        let samples = wav::pcm16_from_base64(&frame.data)?;
        let samples = wav::downmix_to_mono(&samples, frame.num_channels);
        match self.audio_tx.try_send(AudioChunk {
            sample_rate: frame.sample_rate,
            samples,
        }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                warn!("dropping cascade input audio: queue full");
                Ok(())
            }
            Err(TrySendError::Closed(_)) => Err("voice session is not running".to_string()),
        }
    }

    /// Submits typed text as a user turn (after any utterance still being transcribed).
    pub(crate) fn push_text(&self, text: String) -> Result<(), String> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Ok(());
        }
        if text.chars().count() > MAX_TEXT_INPUT_CHARS {
            return Err("text input is too long".to_string());
        }
        self.shared.barge_in();
        match self.utterance_tx.try_send(Utterance::Text(text)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("voice session is busy; try again".to_string()),
            Err(TrySendError::Closed(_)) => Err("voice session is not running".to_string()),
        }
    }

    /// Speaks client-provided text (`appendSpeech`).
    pub(crate) fn push_speech(&self, text: &str) -> Result<(), String> {
        let mut splitter = SentenceSplitter::default();
        let mut sentences = splitter.push(text);
        sentences.extend(splitter.finish());
        let generation = self.shared.generation.load(Ordering::Acquire);
        for sentence in sentences {
            self.enqueue(SpeechJob::Sentence {
                generation,
                item_id: None,
                text: sentence,
            })?;
        }
        self.enqueue(SpeechJob::MessageDone {
            generation,
            item_id: None,
        })
    }

    fn enqueue(&self, job: SpeechJob) -> Result<(), String> {
        if !self.shared.options.speak {
            return Ok(());
        }
        let is_sentence = matches!(job, SpeechJob::Sentence { .. });
        if is_sentence {
            self.shared.pending_speech.fetch_add(1, Ordering::AcqRel);
        }
        match self.speech_tx.try_send(job) {
            Ok(()) => Ok(()),
            Err(error) => {
                if is_sentence {
                    self.shared.pending_speech.fetch_sub(1, Ordering::AcqRel);
                }
                match error {
                    TrySendError::Full(_) => Err("speech queue is full; skipping".to_string()),
                    TrySendError::Closed(_) => Err("voice session is not running".to_string()),
                }
            }
        }
    }

    /// An assistant message started streaming (with any text it already has).
    pub(crate) fn message_started(&self, item_id: &str, initial_text: &str) -> Result<(), String> {
        let generation = self.shared.generation.load(Ordering::Acquire);
        let order = self.shared.next_order.fetch_add(1, Ordering::AcqRel);
        {
            let mut messages = self
                .shared
                .messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if messages.len() >= MAX_TRACKED_MESSAGES
                && let Some(oldest) = messages
                    .iter()
                    .min_by_key(|(_, message)| message.order)
                    .map(|(id, _)| id.clone())
            {
                messages.remove(&oldest);
            }
            messages.insert(
                item_id.to_string(),
                TrackedMessage {
                    splitter: SentenceSplitter::default(),
                    generation,
                    received_text: false,
                    order,
                },
            );
        }
        if initial_text.is_empty() {
            return Ok(());
        }
        self.message_delta(item_id, initial_text)
    }

    pub(crate) fn message_delta(&self, item_id: &str, delta: &str) -> Result<(), String> {
        let (generation, sentences) = {
            let mut messages = self
                .shared
                .messages
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(message) = messages.get_mut(item_id) else {
                return Ok(());
            };
            message.received_text |= !delta.is_empty();
            (message.generation, message.splitter.push(delta))
        };
        self.enqueue_sentences(generation, item_id, sentences)
    }

    /// The message is complete; `full_text` is spoken only if nothing streamed before.
    pub(crate) fn message_completed(&self, item_id: &str, full_text: &str) -> Result<(), String> {
        let tracked = self
            .shared
            .messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(item_id);
        let (generation, sentences) = match tracked {
            Some(mut message) if message.received_text => {
                (message.generation, message.splitter.finish())
            }
            // Never streamed (or never seen): the whole text, in the current generation.
            _ => {
                let mut splitter = SentenceSplitter::default();
                let mut sentences = splitter.push(full_text);
                sentences.extend(splitter.finish());
                (self.shared.generation.load(Ordering::Acquire), sentences)
            }
        };
        self.enqueue_sentences(generation, item_id, sentences)?;
        self.enqueue(SpeechJob::MessageDone {
            generation,
            item_id: Some(item_id.to_string()),
        })
    }

    /// The turn ended without completing these messages (aborted): forget them.
    pub(crate) fn forget_messages(&self) {
        self.shared
            .messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    fn enqueue_sentences(
        &self,
        generation: u64,
        item_id: &str,
        sentences: Vec<String>,
    ) -> Result<(), String> {
        if generation != self.shared.generation.load(Ordering::Acquire) {
            // Muted by a barge-in.
            return Ok(());
        }
        for text in sentences {
            self.enqueue(SpeechJob::Sentence {
                generation,
                item_id: Some(item_id.to_string()),
                text,
            })?;
        }
        Ok(())
    }

    /// Cancels every task and waits up to [`STOP_GRACE`] for them; stragglers are aborted.
    pub(crate) async fn stop(self) {
        self.shared.stop.cancel();
        let CascadeRuntime { tasks, .. } = self;
        for mut task in tasks {
            if tokio::time::timeout(STOP_GRACE, &mut task).await.is_err() {
                warn!("cascade voice task did not stop in time; aborting it");
                task.abort();
            }
        }
    }
}

async fn vad_task<H: CascadeHost>(
    shared: Arc<Shared>,
    host: Arc<H>,
    mut audio_rx: mpsc::Receiver<AudioChunk>,
    utterance_tx: mpsc::Sender<Utterance>,
) {
    let mut vad: Option<EnergyVad> = None;
    loop {
        let chunk = tokio::select! {
            _ = shared.stop.cancelled() => break,
            chunk = audio_rx.recv() => match chunk {
                Some(chunk) => chunk,
                None => break,
            },
        };
        if vad
            .as_ref()
            .is_none_or(|detector| detector.sample_rate() != chunk.sample_rate)
        {
            // A new rate restarts detection (a client switching devices mid-session).
            vad = Some(EnergyVad::new(chunk.sample_rate));
        }
        let Some(detector) = vad.as_mut() else {
            continue;
        };
        for event in detector.push(&chunk.samples) {
            match event {
                VadEvent::SpeechStarted => {
                    let was_speaking = shared.barge_in();
                    host.emit(RealtimeEvent::InputAudioSpeechStarted(
                        RealtimeInputAudioSpeechStarted { item_id: None },
                    ))
                    .await;
                    if was_speaking {
                        host.emit(RealtimeEvent::ResponseCancelled(
                            RealtimeResponseCancelled { response_id: None },
                        ))
                        .await;
                    }
                }
                VadEvent::Utterance(samples) => {
                    let utterance = Utterance::Audio {
                        sample_rate: chunk.sample_rate,
                        samples,
                    };
                    if let Err(TrySendError::Full(_)) = utterance_tx.try_send(utterance) {
                        host.emit(RealtimeEvent::Error(
                            "still transcribing earlier speech; this utterance was dropped"
                                .to_string(),
                        ))
                        .await;
                    }
                }
            }
        }
    }
}

async fn transcription_task<E: AudioEndpoints, H: CascadeHost>(
    shared: Arc<Shared>,
    endpoints: Arc<E>,
    host: Arc<H>,
    mut utterance_rx: mpsc::Receiver<Utterance>,
) {
    loop {
        let utterance = tokio::select! {
            _ = shared.stop.cancelled() => break,
            utterance = utterance_rx.recv() => match utterance {
                Some(utterance) => utterance,
                None => break,
            },
        };
        let text = match utterance {
            Utterance::Text(text) => text,
            Utterance::Audio {
                sample_rate,
                samples,
            } => {
                let wav = wav::encode_pcm16_mono(sample_rate, &samples);
                drop(samples);
                let request = endpoints.transcribe(wav, shared.options.language.clone());
                let result = tokio::select! {
                    _ = shared.stop.cancelled() => break,
                    result = tokio::time::timeout(TRANSCRIPTION_TIMEOUT, request) => result,
                };
                let text = match result {
                    Ok(Ok(text)) => text.trim().to_string(),
                    Ok(Err(error)) => {
                        host.emit(RealtimeEvent::Error(error.to_string())).await;
                        continue;
                    }
                    Err(_) => {
                        host.emit(RealtimeEvent::Error(
                            "transcription failed: timed out".to_string(),
                        ))
                        .await;
                        continue;
                    }
                };
                if text.is_empty() {
                    continue;
                }
                host.emit(RealtimeEvent::InputTranscriptDone(RealtimeTranscriptDone {
                    text: text.clone(),
                }))
                .await;
                text
            }
        };
        if shared.stop.is_cancelled() {
            break;
        }
        if let Err(error) = host.submit_user_text(text).await {
            host.emit(RealtimeEvent::Error(error)).await;
        }
    }
}

async fn speech_task<E: AudioEndpoints, H: CascadeHost>(
    shared: Arc<Shared>,
    endpoints: Arc<E>,
    host: Arc<H>,
    mut speech_rx: mpsc::Receiver<SpeechJob>,
) {
    let chunk_samples = (wav::OUTPUT_SAMPLE_RATE * OUTPUT_CHUNK_MS / 1000) as usize;
    let mut spoken: HashMap<Option<String>, String> = HashMap::new();
    loop {
        let job = tokio::select! {
            _ = shared.stop.cancelled() => break,
            job = speech_rx.recv() => match job {
                Some(job) => job,
                None => break,
            },
        };
        let current = shared.generation.load(Ordering::Acquire);
        let is_sentence = matches!(job, SpeechJob::Sentence { .. });
        if job.generation() != current {
            if is_sentence {
                shared.pending_speech.fetch_sub(1, Ordering::AcqRel);
            }
            continue;
        }
        match job {
            SpeechJob::MessageDone { item_id, .. } => {
                if let Some(text) = spoken.remove(&item_id) {
                    host.emit(RealtimeEvent::OutputTranscriptDone(
                        RealtimeTranscriptDone { text },
                    ))
                    .await;
                }
            }
            SpeechJob::Sentence { item_id, text, .. } => {
                let barge = shared.barge_token();
                let spoke = speak_sentence(
                    &shared,
                    endpoints.as_ref(),
                    host.as_ref(),
                    &barge,
                    item_id.clone(),
                    &text,
                    chunk_samples,
                )
                .await;
                shared.pending_speech.fetch_sub(1, Ordering::AcqRel);
                // Only audio that fully went out counts toward the spoken transcript.
                if spoke && !barge.is_cancelled() {
                    let entry = spoken.entry(item_id).or_default();
                    if !entry.is_empty() {
                        entry.push(' ');
                    }
                    entry.push_str(&text);
                }
            }
        }
        // Transcripts of muted messages never get their done job: keep the map small.
        if spoken.len() > MAX_TRACKED_MESSAGES {
            spoken.clear();
        }
    }
}

async fn speak_sentence<E: AudioEndpoints, H: CascadeHost>(
    shared: &Shared,
    endpoints: &E,
    host: &H,
    barge: &CancellationToken,
    item_id: Option<String>,
    text: &str,
    chunk_samples: usize,
) -> bool {
    let request = endpoints.speak(text.to_string(), shared.options.voice.clone());
    let result = tokio::select! {
        _ = barge.cancelled() => return false,
        result = tokio::time::timeout(SPEECH_TIMEOUT, request) => result,
    };
    let wav_bytes = match result {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            host.emit(RealtimeEvent::Error(error.to_string())).await;
            return false;
        }
        Err(_) => {
            host.emit(RealtimeEvent::Error("speech failed: timed out".to_string()))
                .await;
            return false;
        }
    };
    let samples = match wav::to_output_pcm(&wav_bytes) {
        Ok(samples) => samples,
        Err(error) => {
            host.emit(RealtimeEvent::Error(format!("speech failed: {error}")))
                .await;
            return false;
        }
    };
    host.emit(RealtimeEvent::OutputTranscriptDelta(
        RealtimeTranscriptDelta {
            delta: text.to_string(),
        },
    ))
    .await;
    for chunk in samples.chunks(chunk_samples.max(1)) {
        if barge.is_cancelled() {
            return false;
        }
        host.emit(RealtimeEvent::AudioOut(RealtimeAudioFrame {
            data: wav::pcm16_to_base64(chunk),
            sample_rate: wav::OUTPUT_SAMPLE_RATE,
            num_channels: 1,
            samples_per_channel: Some(chunk.len() as u32),
            item_id: item_id.clone(),
        }))
        .await;
    }
    true
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
