use std::sync::Arc;

use super::SessionTask;
use super::SessionTaskResult;
use super::emit_compact_metric;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use codex_features::Feature;
use codex_model_provider::RemoteCompactionSupport;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub(crate) struct CompactTask {
    /// The user's own focus for the summary (`/compact <instructions>`).
    instructions: Option<String>,
    /// Set for "summarize from here": only this turn and later ones are summarized.
    from_turn_id: Option<String>,
}

impl CompactTask {
    pub(crate) fn new(instructions: Option<String>) -> Self {
        let instructions = instructions
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        Self {
            instructions,
            from_turn_id: None,
        }
    }

    /// A partial compaction from `from_turn_id` on.
    pub(crate) fn from_turn(instructions: Option<String>, from_turn_id: String) -> Self {
        Self {
            from_turn_id: Some(from_turn_id),
            ..Self::new(instructions)
        }
    }
}

impl SessionTask for CompactTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Compact
    }

    fn span_name(&self) -> &'static str {
        "session_task.compact"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        _cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        let _profile_guard = ctx.turn_timing_state.begin_compaction();
        if let Some(from_turn_id) = self.from_turn_id.clone() {
            return self.run_partial(session, ctx, from_turn_id).await;
        }
        if ctx.config.features.enabled(Feature::TokenBudget) {
            crate::compact_token_budget::run_manual_compact_task(session, ctx).await?;
            return Ok(None);
        }

        let result = match ctx.provider.capabilities().remote_compaction {
            RemoteCompactionSupport::V2 => {
                emit_compact_metric(
                    &session.services.session_telemetry,
                    "remote_v2",
                    /*manual*/ true,
                );
                crate::compact_remote_v2::run_remote_compact_task(session.clone(), ctx).await
            }
            RemoteCompactionSupport::Unsupported => {
                emit_compact_metric(
                    &session.services.session_telemetry,
                    "local",
                    /*manual*/ true,
                );
                let base = ctx
                    .config
                    .compact_prompt
                    .as_deref()
                    .unwrap_or(crate::compact::SUMMARIZATION_PROMPT);
                let input = vec![UserInput::Text {
                    text: crate::compact::prompt_with_instructions(
                        base,
                        self.instructions.as_deref(),
                    ),
                    // Compaction prompt is synthesized; no UI element ranges to preserve.
                    text_elements: Vec::new(),
                }];
                crate::compact::run_compact_task(session.clone(), ctx, input).await
            }
        };
        if let Err(err) = result
            && matches!(err.details(), CodexErrorDetails::TurnAborted)
        {
            return Err(err);
        }
        Ok(None)
    }
}

impl CompactTask {
    async fn run_partial(
        &self,
        session: Arc<Session>,
        ctx: Arc<TurnContext>,
        from_turn_id: String,
    ) -> SessionTaskResult {
        // Remote or token-budget compaction cannot keep a verbatim prefix: refuse, never fall back
        // to compacting the whole thread.
        if let Err(err) = crate::compact::ensure_partial_compaction_supported(ctx.as_ref()) {
            session.emit_turn_started(&ctx).await;
            session
                .send_event(
                    &ctx,
                    EventMsg::Error(err.to_error_event(/*message_prefix*/ None)),
                )
                .await;
            return Ok(None);
        }
        emit_compact_metric(
            &session.services.session_telemetry,
            "local_partial",
            /*manual*/ true,
        );
        let input = vec![UserInput::Text {
            text: crate::compact::prompt_with_instructions(
                crate::compact::PARTIAL_SUMMARIZATION_PROMPT,
                self.instructions.as_deref(),
            ),
            // Compaction prompt is synthesized; no UI element ranges to preserve.
            text_elements: Vec::new(),
        }];
        let result =
            crate::compact::run_partial_compact_task(session.clone(), ctx, input, from_turn_id)
                .await;
        if let Err(err) = result
            && matches!(err.details(), CodexErrorDetails::TurnAborted)
        {
            return Err(err);
        }
        Ok(None)
    }
}
