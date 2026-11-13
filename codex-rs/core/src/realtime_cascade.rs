//! Cascade voice mode of thread realtime (`thread/realtime/start` with the cascade transport).
//!
//! Instead of a realtime upstream, the session cuts input speech into utterances with a local
//! energy VAD, transcribes each one through the model provider's `audio/transcriptions` endpoint
//! and submits the text as a normal user turn (the path realtime handoffs use). The assistant
//! messages of the thread's turns are split into sentences as they stream and spoken through the
//! provider's `audio/speech` endpoint; the audio leaves as `RealtimeEvent::AudioOut`, so clients
//! receive the usual `thread/realtime/*` notifications. See `runtime.rs` for the loops and their
//! bounds.
//!
//! This module is self-contained: the realtime handlers only branch into it while a cascade
//! session is active.

mod endpoints;
mod runtime;
mod sentences;
mod vad;
mod wav;

use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::ConversationTextRole;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RealtimeAudioFrame;
use codex_protocol::protocol::RealtimeConversationClosedEvent;
use codex_protocol::protocol::RealtimeConversationRealtimeEvent;
use codex_protocol::protocol::RealtimeConversationStartedEvent;
use codex_protocol::protocol::RealtimeConversationVersion;
use codex_protocol::protocol::RealtimeEvent;
use codex_protocol::realtime_cascade::CascadeStartParams;
use codex_protocol::realtime_cascade::CascadeVoicesList;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tracing::info;
use tracing::warn;

use crate::session::session::Session;
use endpoints::AudioEndpoints;
use endpoints::ProviderAudioEndpoints;
use endpoints::VOICES_TIMEOUT;
use runtime::CascadeHost;
use runtime::CascadeOptions;
use runtime::CascadeRuntime;

/// Longest accepted voice id and language hint.
const MAX_OPTION_CHARS: usize = 64;

struct ActiveCascade {
    sub_id: String,
    runtime: CascadeRuntime,
}

/// The thread's cascade session, if any. Owned by the realtime conversation manager.
pub(crate) struct CascadeManager {
    state: Mutex<Option<ActiveCascade>>,
    /// Lock-free check for the event path, which runs for every event of every turn.
    active: AtomicBool,
}

impl Default for CascadeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl CascadeManager {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(None),
            active: AtomicBool::new(false),
        }
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Stops the session, if any; returns its submission id.
    pub(crate) async fn stop(&self) -> Option<String> {
        let active = self.state.lock().await.take();
        self.active.store(false, Ordering::Release);
        let active = active?;
        active.runtime.stop().await;
        Some(active.sub_id)
    }

    /// Feeds the thread's events: assistant messages are spoken.
    pub(crate) async fn observe(&self, msg: &EventMsg) {
        if !self.is_active() {
            return;
        }
        let state = self.state.lock().await;
        let Some(active) = state.as_ref() else {
            return;
        };
        let result = match msg {
            EventMsg::ItemStarted(event) => match &event.item {
                TurnItem::AgentMessage(item) => active
                    .runtime
                    .message_started(&item.id, &agent_message_text(item)),
                _ => Ok(()),
            },
            EventMsg::AgentMessageContentDelta(event) => {
                active.runtime.message_delta(&event.item_id, &event.delta)
            }
            EventMsg::ItemCompleted(event) => match &event.item {
                TurnItem::AgentMessage(item) => active
                    .runtime
                    .message_completed(&item.id, &agent_message_text(item)),
                _ => Ok(()),
            },
            EventMsg::TurnAborted(_) => {
                active.runtime.forget_messages();
                Ok(())
            }
            _ => Ok(()),
        };
        if let Err(error) = result {
            warn!("cascade voice: {error}");
        }
    }
}

fn agent_message_text(item: &AgentMessageItem) -> String {
    item.content
        .iter()
        .map(|entry| match entry {
            AgentMessageContent::Text { text } => text.as_str(),
        })
        .collect()
}

/// The session side of the tasks; holds the session weakly so a dropped thread ends them.
struct SessionHost {
    session: Weak<Session>,
    sub_id: String,
}

impl CascadeHost for SessionHost {
    async fn emit(&self, event: RealtimeEvent) {
        let Some(session) = self.session.upgrade() else {
            return;
        };
        session
            .send_event_raw(Event {
                id: self.sub_id.clone(),
                msg: EventMsg::RealtimeConversationRealtime(RealtimeConversationRealtimeEvent {
                    payload: event,
                }),
            })
            .await;
    }

    async fn submit_user_text(&self, text: String) -> Result<(), String> {
        let Some(session) = self.session.upgrade() else {
            return Err("the thread is closed".to_string());
        };
        // Bound the layout of the turn-admission future, as the realtime handoff path does.
        Box::pin(session.route_realtime_text_input(text))
            .await
            .map_err(str::to_string)
    }
}

fn checked_option(name: &str, value: Option<String>) -> CodexResult<Option<String>> {
    let Some(value) = value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    let plain = value.chars().count() <= MAX_OPTION_CHARS
        && value
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/'));
    if !plain {
        return Err(CodexErr::InvalidRequest(format!(
            "cascade {name} must be at most {MAX_OPTION_CHARS} letters, digits or _-.:/"
        )));
    }
    Ok(Some(value))
}

async fn provider_endpoints(
    sess: &Session,
) -> CodexResult<ProviderAudioEndpoints<codex_api::ReqwestTransport>> {
    let setup = sess
        .services
        .model_client
        .configured_provider_setup()
        .await?;
    let transport = setup
        .http_transport(endpoints::SPEECH_PATH)
        .map_err(|error| CodexErr::InvalidRequest(format!("voice transport: {error}")))?;
    Ok(ProviderAudioEndpoints::new(
        transport,
        setup.api_provider,
        setup.api_auth,
    ))
}

async fn send_event(sess: &Session, sub_id: &str, msg: EventMsg) {
    sess.send_event_raw(Event {
        id: sub_id.to_string(),
        msg,
    })
    .await;
}

async fn send_error(sess: &Session, sub_id: &str, message: String) {
    send_event(
        sess,
        sub_id,
        EventMsg::RealtimeConversationRealtime(RealtimeConversationRealtimeEvent {
            payload: RealtimeEvent::Error(message),
        }),
    )
    .await;
}

async fn send_closed(sess: &Session, sub_id: &str, reason: &str) {
    send_event(
        sess,
        sub_id,
        EventMsg::RealtimeConversationClosed(RealtimeConversationClosedEvent {
            reason: Some(reason.to_string()),
        }),
    )
    .await;
}

/// `Op::RealtimeCascadeStart`: replaces any realtime or cascade session on the thread.
pub(crate) async fn handle_start(sess: &Arc<Session>, sub_id: String, params: CascadeStartParams) {
    let prepared = async {
        let voice = checked_option("voice", params.voice)?;
        let language = checked_option("language", params.language)?;
        let endpoints = provider_endpoints(sess).await?;
        CodexResult::Ok((voice, language, endpoints))
    }
    .await;
    let (voice, language, endpoints) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            send_error(
                sess,
                &sub_id,
                format!("voice session could not start: {error}"),
            )
            .await;
            return;
        }
    };
    if sess.conversation.running_state().await.is_some() {
        crate::realtime_conversation::handle_detach(sess, /*expected*/ None, &sub_id).await;
    }
    let manager = &sess.conversation.cascade;
    if let Some(previous_sub_id) = manager.stop().await {
        send_closed(sess, &previous_sub_id, "requested").await;
    }
    let host = Arc::new(SessionHost {
        session: Arc::downgrade(sess),
        sub_id: sub_id.clone(),
    });
    let runtime = CascadeRuntime::spawn(
        Arc::new(endpoints),
        host,
        CascadeOptions {
            voice,
            language,
            speak: params.speak,
        },
    );
    {
        let mut state = manager.state.lock().await;
        *state = Some(ActiveCascade {
            sub_id: sub_id.clone(),
            runtime,
        });
        manager.active.store(true, Ordering::Release);
    }
    info!("cascade voice session started");
    send_event(
        sess,
        &sub_id,
        EventMsg::RealtimeConversationStarted(RealtimeConversationStartedEvent {
            realtime_session_id: params.realtime_session_id,
            version: RealtimeConversationVersion::default(),
        }),
    )
    .await;
}

/// `appendAudio` while a cascade session runs; `false` when none does (the realtime path goes on).
pub(crate) async fn handle_audio(sess: &Session, sub_id: &str, frame: &RealtimeAudioFrame) -> bool {
    let manager = &sess.conversation.cascade;
    if !manager.is_active() {
        return false;
    }
    let result = match manager.state.lock().await.as_ref() {
        Some(active) => active.runtime.push_audio(frame),
        None => return false,
    };
    if let Err(error) = result {
        send_error(sess, sub_id, error).await;
    }
    true
}

/// `appendText` while a cascade session runs: user text becomes a turn.
pub(crate) async fn handle_text(
    sess: &Session,
    sub_id: &str,
    text: &str,
    role: ConversationTextRole,
) -> bool {
    let manager = &sess.conversation.cascade;
    if !manager.is_active() {
        return false;
    }
    let result = match manager.state.lock().await.as_ref() {
        Some(_) if role != ConversationTextRole::User => {
            Err("voice sessions accept only user text".to_string())
        }
        Some(active) => active.runtime.push_text(text.to_string()),
        None => return false,
    };
    if let Err(error) = result {
        send_error(sess, sub_id, error).await;
    }
    true
}

/// `appendSpeech` while a cascade session runs: the text is spoken as is.
pub(crate) async fn handle_speech(sess: &Session, sub_id: &str, text: &str) -> bool {
    let manager = &sess.conversation.cascade;
    if !manager.is_active() {
        return false;
    }
    let result = match manager.state.lock().await.as_ref() {
        Some(active) => active.runtime.push_speech(text),
        None => return false,
    };
    if let Err(error) = result {
        send_error(sess, sub_id, error).await;
    }
    true
}

/// `stop` while a cascade session runs: stops it and reports the closure.
pub(crate) async fn handle_close(sess: &Session) -> bool {
    let manager = &sess.conversation.cascade;
    if !manager.is_active() {
        return false;
    }
    match manager.stop().await {
        Some(sub_id) => {
            info!("cascade voice session stopped");
            send_closed(sess, &sub_id, "requested").await;
            true
        }
        None => false,
    }
}

/// `Op::RealtimeCascadeListVoices`: answered from a task so the submission loop never waits on
/// the network.
pub(crate) async fn handle_list_voices(
    sess: &Arc<Session>,
    reply: oneshot::Sender<CodexResult<CascadeVoicesList>>,
) {
    let endpoints = match provider_endpoints(sess).await {
        Ok(endpoints) => endpoints,
        Err(error) => {
            let _ = reply.send(Err(error));
            return;
        }
    };
    tokio::spawn(async move {
        let result = match tokio::time::timeout(VOICES_TIMEOUT, endpoints.voices()).await {
            Ok(Ok(voices)) => Ok(voices),
            Ok(Err(error)) => Err(CodexErr::InvalidRequest(error.to_string())),
            Err(_) => Err(CodexErr::InvalidRequest(
                "voice list failed: timed out".to_string(),
            )),
        };
        let _ = reply.send(result);
    });
}
