//! Chat Completions transport for [`ModelClientSession`].
//!
//! Reuses the Responses request builder and converts the request inside
//! `codex-api`, so downstream turn handling sees the same `ResponseEvent`s for
//! both wire APIs. Chat never uses WebSockets, request compression, workspace
//! routing, or OpenAI-only request metadata.

use super::*;
use codex_api::ChatCompletionsClient as ApiChatCompletionsClient;

const CHAT_COMPLETIONS_ENDPOINT: &str = "/chat/completions";

impl ModelClientSession {
    /// Streams a turn via an OpenAI-compatible Chat Completions endpoint.
    #[allow(clippy::too_many_arguments)]
    #[instrument(
        name = "model_client.stream_chat_api",
        level = "info",
        skip_all,
        fields(
            model = %model_info.slug,
            wire_api = %self.client.state.provider.info().wire_api,
            transport = "chat_completions_http",
            http.method = "POST",
            api.path = CHAT_COMPLETIONS_ENDPOINT
        )
    )]
    pub(super) async fn stream_chat_api(
        &self,
        prompt: &Arc<Prompt>,
        model_info: &ModelInfo,
        session_telemetry: &SessionTelemetry,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        responses_metadata: &CodexResponsesMetadata,
        inference_trace: &InferenceTraceContext,
    ) -> Result<ResponseStream> {
        let auth_manager = self.client.state.provider.auth_manager();
        let mut auth_recovery = auth_manager
            .as_ref()
            .map(AuthManager::unauthorized_recovery);
        let mut provider_auth_recovery_attempted = false;
        let mut pending_retry = PendingUnauthorizedRetry::default();
        loop {
            let client_setup = self
                .client
                .current_client_setup(ClientRouting::ConfiguredProvider)
                .await?;
            let include_internal = self
                .client
                .state
                .provider
                .include_internal_metadata(&client_setup.api_provider);
            let transport = client_setup.http_transport(CHAT_COMPLETIONS_ENDPOINT)?;
            let request_auth_context = AuthRequestTelemetryContext::new(
                client_setup.auth.as_ref().map(CodexAuth::auth_mode),
                client_setup.api_auth.as_ref(),
                client_setup.agent_identity_telemetry.clone(),
                pending_retry,
            )
            .map_err(|error| self.client.state.provider.map_api_error(error))?;
            let (request_telemetry, sse_telemetry) = Self::build_streaming_telemetry(
                session_telemetry,
                request_auth_context,
                RequestRouteTelemetry::for_endpoint(CHAT_COMPLETIONS_ENDPOINT),
                self.client.state.auth_env_telemetry.clone(),
            );
            let mut options = ResponsesHttpExecutor::new(&self.client)
                .build_responses_options(
                    &self.turn_state,
                    responses_metadata,
                    Compression::None,
                    model_info.use_responses_lite,
                )
                .await;
            let mut request = self.client.build_responses_request(
                prompt,
                model_info,
                effort.clone(),
                summary,
                service_tier.clone(),
                responses_metadata,
                include_internal,
            )?;
            self.client
                .prepare_response_items_for_request(&mut request.input);
            let request_session_telemetry =
                session_telemetry_for_request(session_telemetry, &request);
            options.extra_headers.extend(
                self.client
                    .responses_headers(client_setup.auth.as_ref(), &model_info.slug),
            );
            let interceptors = crate::model_request::prepare(
                &self.client.request_contributors,
                &self.client.state.thread_id.to_string(),
                &model_info.slug,
                codex_extension_api::ModelRequestKind::Generation,
                &mut request.client_metadata,
            );
            let inference_trace_attempt = inference_trace.start_attempt();
            inference_trace_attempt.add_request_headers(&mut options.extra_headers);
            if let Some(input) = tool_metadata::bounded_input(&request, &request.input) {
                if let Some(recorder) = &self.client.executed_tool_calls {
                    recorder.invalidate_wire_inventory_loss(&request.input, &input);
                }
                request.input = input;
            }
            let client = ApiChatCompletionsClient::new(
                transport,
                client_setup.api_provider,
                client_setup.api_auth,
            )
            .with_telemetry(Some(request_telemetry), Some(sse_telemetry));

            let stream_result = client
                .stream_request_with_observer(&request, options, |wire| {
                    inference_trace_attempt.record_started(
                        wire.clone(),
                        Arc::clone(prompt),
                        || Ok(None),
                    );
                })
                .await;
            match stream_result {
                Ok(stream) => {
                    return Ok(map_response_stream(
                        stream,
                        request_session_telemetry,
                        inference_trace_attempt,
                        &self.client.state,
                        interceptors,
                        /*last_response_tx*/ None,
                    ));
                }
                Err(ApiError::Transport(unauthorized_transport))
                    if self
                        .client
                        .state
                        .provider
                        .is_recoverable_auth_error(&unauthorized_transport) =>
                {
                    let response_debug_context =
                        extract_response_debug_context(&unauthorized_transport);
                    inference_trace_attempt.record_failed(
                        &unauthorized_transport,
                        response_debug_context.request_id.as_deref(),
                        /*output_items*/ &[],
                    );
                    pending_retry = PendingUnauthorizedRetry::from_recovery(
                        handle_unauthorized(
                            unauthorized_transport,
                            &mut auth_recovery,
                            &mut provider_auth_recovery_attempted,
                            session_telemetry,
                            &self.client.state.provider,
                            self.client.event_sender.as_ref(),
                            responses_metadata.turn_id.as_deref(),
                        )
                        .await?,
                    );
                    continue;
                }
                Err(err) => {
                    let response_debug_context =
                        extract_response_debug_context_from_api_error(&err);
                    let err = self.client.state.provider.map_api_error(err);
                    inference_trace_attempt.record_failed(
                        &err,
                        response_debug_context.request_id.as_deref(),
                        /*output_items*/ &[],
                    );
                    return Err(err);
                }
            }
        }
    }
}
