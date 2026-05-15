//! Session- and turn-scoped helpers for talking to model provider APIs.
//!
//! `ModelClient` is intended to live for the lifetime of a Codex session and holds the stable
//! configuration and state needed to talk to a provider (auth, provider selection, conversation id,
//! and transport fallback state).
//!
//! Per-turn settings (model selection, reasoning controls, telemetry context, and turn metadata)
//! are passed explicitly to streaming and unary methods so that the turn lifetime is visible at the
//! call site.
//!
//! A [`ModelClientSession`] is created per turn and is used to stream one or more Responses API
//! requests during that turn. It caches a Responses WebSocket connection (opened lazily) and stores
//! per-turn state such as the `x-codex-turn-state` token used for sticky routing.
//!
//! WebSocket prewarm is a v2-only `response.create` with `generate=false`; it waits for completion
//! so the next request can reuse the same connection and `previous_response_id`.
//!
//! Turn execution performs prewarm as a best-effort step before the first stream request so the
//! subsequent request can reuse the same connection.
//!
//! ## Retry-Budget Tradeoff
//!
//! WebSocket prewarm is treated as the first websocket connection attempt for a turn. If it
//! fails, normal stream retry/fallback logic handles recovery on the same turn.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_api::AnthropicClient as ApiAnthropicClient;
use codex_api::ApiError;
use codex_api::AuthProvider;
use codex_api::ChatCompletionsClient as ApiChatCompletionsClient;
use codex_api::ChatCompletionsRequest;
use codex_api::ChatMessage;
use codex_api::CompactClient as ApiCompactClient;
use codex_api::CompactionInput as ApiCompactionInput;
use codex_api::Compression;
use codex_api::MemoriesClient as ApiMemoriesClient;
use codex_api::MemorySummarizeInput as ApiMemorySummarizeInput;
use codex_api::MemorySummarizeOutput as ApiMemorySummarizeOutput;
use codex_api::Provider as ApiProvider;
use codex_api::RawMemory as ApiRawMemory;
use codex_api::RealtimeCallClient as ApiRealtimeCallClient;
use codex_api::RealtimeSessionConfig as ApiRealtimeSessionConfig;
use codex_api::Reasoning;
use codex_api::RequestTelemetry;
use codex_api::ReqwestTransport;
use codex_api::ResponseCreateWsRequest;
use codex_api::ResponsesApiRequest;
use codex_api::ResponsesClient as ApiResponsesClient;
use codex_api::ResponsesOptions as ApiResponsesOptions;
use codex_api::ResponsesWebsocketClient as ApiWebSocketResponsesClient;
use codex_api::ResponsesWebsocketConnection as ApiWebSocketConnection;
use codex_api::ResponsesWsRequest;
use codex_api::SharedAuthProvider;
use codex_api::SseTelemetry;
use codex_api::TransportError;
use codex_api::WebsocketTelemetry;
use codex_api::auth_header_telemetry;
use codex_api::build_session_headers;
use codex_api::create_text_param_for_request;
use codex_api::response_create_client_metadata;
use codex_app_server_protocol::AuthMode;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::RefreshTokenError;
use codex_login::UnauthorizedRecovery;
use codex_login::default_client::build_reqwest_client;
use codex_otel::SessionTelemetry;
use codex_otel::current_span_w3c_trace_context;

use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary as ReasoningSummaryConfig;
use codex_protocol::config_types::Verbosity as VerbosityConfig;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort as ReasoningEffortConfig;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::W3cTraceContext;
use codex_rollout_trace::CompactionTraceContext;
use codex_rollout_trace::InferenceTraceAttempt;
use codex_rollout_trace::InferenceTraceContext;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolSpec;
use codex_tools::create_tools_json_for_chat_completions;
use codex_tools::create_tools_json_for_responses_api;
use eventsource_stream::Event;
use eventsource_stream::EventStreamError;
use futures::StreamExt;
use http::HeaderMap as ApiHeaderMap;
use http::HeaderValue;
use http::StatusCode as HttpStatusCode;
use reqwest::StatusCode;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::TryRecvError;
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::Error;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use tracing::instrument;
use tracing::trace;
use tracing::warn;

use crate::attestation::AttestationContext;
use crate::attestation::AttestationProvider;
use crate::attestation::X_OAI_ATTESTATION_HEADER;
use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::client_common::ResponseStream;
use crate::feedback_tags;
use crate::util::emit_feedback_auth_recovery_tags;
use codex_api::map_api_error;
use codex_feedback::FeedbackRequestTags;
use codex_feedback::emit_feedback_request_tags_with_auth_env;
use codex_login::auth_env_telemetry::AuthEnvTelemetry;
use codex_login::auth_env_telemetry::collect_auth_env_telemetry;
use codex_model_provider::SharedModelProvider;
use codex_model_provider::create_model_provider;
#[cfg(test)]
use codex_model_provider_info::DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_response_debug_context::extract_response_debug_context;
use codex_response_debug_context::extract_response_debug_context_from_api_error;
use codex_response_debug_context::telemetry_api_error_message;
use codex_response_debug_context::telemetry_transport_error_message;

pub const OPENAI_BETA_HEADER: &str = "OpenAI-Beta";
pub const X_CODEX_INSTALLATION_ID_HEADER: &str = "x-codex-installation-id";
pub const X_CODEX_TURN_STATE_HEADER: &str = "x-codex-turn-state";
pub const X_CODEX_TURN_METADATA_HEADER: &str = "x-codex-turn-metadata";
pub const X_CODEX_PARENT_THREAD_ID_HEADER: &str = "x-codex-parent-thread-id";
pub const X_CODEX_WINDOW_ID_HEADER: &str = "x-codex-window-id";
pub const X_OPENAI_MEMGEN_REQUEST_HEADER: &str = "x-openai-memgen-request";
pub const X_OPENAI_SUBAGENT_HEADER: &str = "x-openai-subagent";
pub const X_RESPONSESAPI_INCLUDE_TIMING_METRICS_HEADER: &str =
    "x-responsesapi-include-timing-metrics";
const X_CODEX_WS_STREAM_REQUEST_START_MS_CLIENT_METADATA_KEY: &str =
    "x-codex-ws-stream-request-start-ms";
const RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE: &str = "responses_websockets=2026-02-06";
const RESPONSES_ENDPOINT: &str = "/responses";
const RESPONSES_COMPACT_ENDPOINT: &str = "/responses/compact";
// `/responses/compact` is unary, so the timeout covers the full response rather than one idle
// period between stream events.
const COMPACT_REQUEST_TIMEOUT_IDLE_MULTIPLIER: u32 = 4;
const MEMORIES_SUMMARIZE_ENDPOINT: &str = "/memories/trace_summarize";
#[cfg(test)]
pub(crate) const WEBSOCKET_CONNECT_TIMEOUT: Duration =
    Duration::from_millis(DEFAULT_WEBSOCKET_CONNECT_TIMEOUT_MS);

pub(crate) struct CompactConversationRequestSettings {
    pub(crate) effort: Option<ReasoningEffortConfig>,
    pub(crate) summary: ReasoningSummaryConfig,
    pub(crate) service_tier: Option<String>,
}

/// Session-scoped state shared by all [`ModelClient`] clones.
///
/// This is intentionally kept minimal so `ModelClient` does not need to hold a full `Config`. Most
/// configuration is per turn and is passed explicitly to streaming/unary methods.
#[derive(Debug)]
struct ModelClientState {
    session_id: SessionId,
    thread_id: ThreadId,
    window_generation: AtomicU64,
    installation_id: String,
    provider: SharedModelProvider,
    auth_env_telemetry: AuthEnvTelemetry,
    session_source: SessionSource,
    model_verbosity: Option<VerbosityConfig>,
    enable_request_compression: bool,
    include_timing_metrics: bool,
    beta_features_header: Option<String>,
    include_attestation: bool,
    attestation_provider: Option<Arc<dyn AttestationProvider>>,
    disable_websockets: AtomicBool,
    cached_websocket_session: StdMutex<WebsocketSession>,
}

/// Resolved API client setup for a single request attempt.
///
/// Keeping this as a single bundle ensures prewarm and normal request paths
/// share the same auth/provider setup flow.
struct CurrentClientSetup {
    auth: Option<CodexAuth>,
    api_provider: ApiProvider,
    api_auth: SharedAuthProvider,
}

#[derive(Clone, Copy)]
struct RequestRouteTelemetry {
    endpoint: &'static str,
}

impl RequestRouteTelemetry {
    fn for_endpoint(endpoint: &'static str) -> Self {
        Self { endpoint }
    }
}

/// A session-scoped client for model-provider API calls.
///
/// This holds configuration and state that should be shared across turns within a Codex session
/// (auth, provider selection, thread id, and transport fallback state).
///
/// WebSocket fallback is session-scoped: once a turn activates the HTTP fallback, subsequent turns
/// will also use HTTP for the remainder of the session.
///
/// Turn-scoped settings (model selection, reasoning controls, telemetry context, and turn
/// metadata) are passed explicitly to the relevant methods to keep turn lifetime visible at the
/// call site.
#[derive(Debug, Clone)]
pub struct ModelClient {
    state: Arc<ModelClientState>,
}

/// A turn-scoped streaming session created from a [`ModelClient`].
///
/// The session establishes a Responses WebSocket connection lazily and reuses it across multiple
/// requests within the turn. It also caches per-turn state:
///
/// - The last full request, so subsequent calls can reuse incremental websocket request payloads
///   only when the current request is an incremental extension of the previous one.
/// - The `x-codex-turn-state` sticky-routing token, which must be replayed for all requests within
///   the same turn.
///
/// Create a fresh `ModelClientSession` for each Codex turn. Reusing it across turns would replay
/// the previous turn's sticky-routing token into the next turn, which violates the client/server
/// contract and can cause routing bugs.
pub struct ModelClientSession {
    client: ModelClient,
    websocket_session: WebsocketSession,
    /// Turn state for sticky routing.
    ///
    /// This is an `OnceLock` that stores the turn state value received from the server
    /// on turn start via the `x-codex-turn-state` response header. Once set, this value
    /// should be sent back to the server in the `x-codex-turn-state` request header for
    /// all subsequent requests within the same turn to maintain sticky routing.
    ///
    /// This is a contract between the client and server: we receive it at turn start,
    /// keep sending it unchanged between turn requests (e.g., for retries, incremental
    /// appends, or continuation requests), and must not send it between different turns.
    turn_state: Arc<OnceLock<String>>,
}

#[derive(Debug, Clone)]
struct LastResponse {
    response_id: String,
    items_added: Vec<ResponseItem>,
}

#[derive(Debug, Default)]
struct WebsocketSession {
    connection: Option<ApiWebSocketConnection>,
    last_request: Option<ResponsesApiRequest>,
    last_response_rx: Option<oneshot::Receiver<LastResponse>>,
    last_response_from_untraced_warmup: bool,
    connection_reused: StdMutex<bool>,
}

impl WebsocketSession {
    fn set_connection_reused(&self, connection_reused: bool) {
        *self
            .connection_reused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = connection_reused;
    }

    fn connection_reused(&self) -> bool {
        *self
            .connection_reused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

enum WebsocketStreamOutcome {
    Stream(ResponseStream),
    FallbackToHttp,
}

/// Result of opening a WebRTC Realtime call.
///
/// The SDP answer goes back to the client. The call id and auth headers stay on the server so the
/// ordinary Realtime WebSocket machinery can join the same in-progress call as a sideband
/// controller.
pub(crate) struct RealtimeWebrtcCallStart {
    pub(crate) sdp: String,
    pub(crate) call_id: String,
    pub(crate) sideband_headers: ApiHeaderMap,
}

/// Reuses the API-auth material that created the WebRTC call for the sideband WebSocket join.
///
/// API-key sessions send that API bearer. ChatGPT-auth sessions send their bearer plus account id;
/// transceiver is responsible for accepting that same call-create identity on the direct
/// `api.openai.com` sideband path.
fn sideband_websocket_auth_headers(api_auth: &dyn AuthProvider) -> ApiHeaderMap {
    let mut headers = ApiHeaderMap::new();
    api_auth.add_auth_headers(&mut headers);
    headers
}

impl ModelClient {
    #[allow(clippy::too_many_arguments)]
    /// Creates a new session-scoped `ModelClient`.
    ///
    /// All arguments are expected to be stable for the lifetime of a Codex session. Per-turn values
    /// are passed to [`ModelClientSession::stream`] (and other turn-scoped methods) explicitly.
    pub fn new(
        auth_manager: Option<Arc<AuthManager>>,
        session_id: SessionId,
        thread_id: ThreadId,
        installation_id: String,
        provider_info: ModelProviderInfo,
        session_source: SessionSource,
        model_verbosity: Option<VerbosityConfig>,
        enable_request_compression: bool,
        include_timing_metrics: bool,
        beta_features_header: Option<String>,
        attestation_provider: Option<Arc<dyn AttestationProvider>>,
    ) -> Self {
        let model_provider = create_model_provider(provider_info, auth_manager);
        let codex_api_key_env_enabled = model_provider
            .auth_manager()
            .as_ref()
            .is_some_and(|manager| manager.codex_api_key_env_enabled());
        let auth_env_telemetry =
            collect_auth_env_telemetry(model_provider.info(), codex_api_key_env_enabled);
        let include_attestation = model_provider.supports_attestation();
        Self {
            state: Arc::new(ModelClientState {
                session_id,
                thread_id,
                window_generation: AtomicU64::new(0),
                installation_id,
                provider: model_provider,
                auth_env_telemetry,
                session_source,
                model_verbosity,
                enable_request_compression,
                include_timing_metrics,
                beta_features_header,
                include_attestation,
                attestation_provider,
                disable_websockets: AtomicBool::new(false),
                cached_websocket_session: StdMutex::new(WebsocketSession::default()),
            }),
        }
    }

    /// Creates a fresh turn-scoped streaming session.
    ///
    /// This constructor does not perform network I/O itself; the session opens a websocket lazily
    /// when the first stream request is issued.
    pub fn new_session(&self) -> ModelClientSession {
        ModelClientSession {
            client: self.clone(),
            websocket_session: self.take_cached_websocket_session(),
            turn_state: Arc::new(OnceLock::new()),
        }
    }

    pub(crate) fn auth_manager(&self) -> Option<Arc<AuthManager>> {
        self.state.provider.auth_manager()
    }

    pub(crate) fn set_window_generation(&self, window_generation: u64) {
        self.state
            .window_generation
            .store(window_generation, Ordering::Relaxed);
        self.store_cached_websocket_session(WebsocketSession::default());
    }

    pub(crate) fn advance_window_generation(&self) {
        self.state.window_generation.fetch_add(1, Ordering::Relaxed);
        self.store_cached_websocket_session(WebsocketSession::default());
    }

    pub(crate) fn current_window_id(&self) -> String {
        let thread_id = self.state.thread_id;
        let window_generation = self.state.window_generation.load(Ordering::Relaxed);
        format!("{thread_id}:{window_generation}")
    }

    fn take_cached_websocket_session(&self) -> WebsocketSession {
        let mut cached_websocket_session = self
            .state
            .cached_websocket_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *cached_websocket_session)
    }

    fn store_cached_websocket_session(&self, websocket_session: WebsocketSession) {
        *self
            .state
            .cached_websocket_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = websocket_session;
    }

    pub(crate) fn force_http_fallback(
        &self,
        session_telemetry: &SessionTelemetry,
        _model_info: &ModelInfo,
    ) -> bool {
        let websocket_enabled = self.responses_websocket_enabled();
        let activated =
            websocket_enabled && !self.state.disable_websockets.swap(true, Ordering::Relaxed);
        if activated {
            warn!("falling back to HTTP");
            session_telemetry.counter(
                "codex.transport.fallback_to_http",
                /*inc*/ 1,
                &[("from_wire_api", "responses_websocket")],
            );
        }

        self.store_cached_websocket_session(WebsocketSession::default());
        activated
    }

    /// Compacts the current conversation history using the Compact endpoint.
    ///
    /// This is a unary call (no streaming) that returns a new list of
    /// `ResponseItem`s representing the compacted transcript.
    ///
    /// The model selection and telemetry context are passed explicitly to keep `ModelClient`
    /// session-scoped.
    pub(crate) async fn compact_conversation_history(
        &self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        settings: CompactConversationRequestSettings,
        session_telemetry: &SessionTelemetry,
        compaction_trace: &CompactionTraceContext,
        turn_metadata_header: Option<&str>,
    ) -> Result<Vec<ResponseItem>> {
        if prompt.input.is_empty() {
            return Ok(Vec::new());
        }
        let client_setup = self.current_client_setup().await?;
        let transport = ReqwestTransport::new(build_reqwest_client());
        let request_telemetry = Self::build_request_telemetry(
            session_telemetry,
            AuthRequestTelemetryContext::new(
                client_setup.auth.as_ref().map(CodexAuth::auth_mode),
                client_setup.api_auth.as_ref(),
                PendingUnauthorizedRetry::default(),
            ),
            RequestRouteTelemetry::for_endpoint(RESPONSES_COMPACT_ENDPOINT),
            self.state.auth_env_telemetry.clone(),
        );
        let request = self.build_responses_request(
            &client_setup.api_provider,
            prompt,
            model_info,
            settings.effort,
            settings.summary,
            settings.service_tier,
        )?;
        let ResponsesApiRequest {
            model,
            instructions,
            input,
            tools,
            parallel_tool_calls,
            reasoning,
            service_tier,
            prompt_cache_key,
            text,
            ..
        } = request;
        let payload = ApiCompactionInput {
            model: &model,
            input: &input,
            instructions: &instructions,
            tools,
            parallel_tool_calls,
            reasoning,
            service_tier: service_tier.as_deref(),
            prompt_cache_key: prompt_cache_key.as_deref(),
            text,
        };

        let mut extra_headers = ApiHeaderMap::new();
        if let Ok(header_value) = HeaderValue::from_str(&self.state.installation_id) {
            extra_headers.insert(X_CODEX_INSTALLATION_ID_HEADER, header_value);
        }
        extra_headers.extend(build_responses_headers(
            self.state.beta_features_header.as_deref(),
            /*turn_state*/ None,
            parse_turn_metadata_header(turn_metadata_header).as_ref(),
        ));
        extra_headers.extend(self.build_responses_identity_headers());
        extra_headers.extend(build_session_headers(
            Some(self.state.session_id.to_string()),
            Some(self.state.thread_id.to_string()),
        ));
        if let Some(header_value) = self.generate_attestation_header_for().await {
            extra_headers.insert(X_OAI_ATTESTATION_HEADER, header_value);
        }
        let compact_request_timeout = client_setup
            .api_provider
            .stream_idle_timeout
            .saturating_mul(COMPACT_REQUEST_TIMEOUT_IDLE_MULTIPLIER);
        let client =
            ApiCompactClient::new(transport, client_setup.api_provider, client_setup.api_auth)
                .with_telemetry(Some(request_telemetry));
        let trace_attempt = compaction_trace.start_attempt(&payload);
        let result = client
            .compact_input(&payload, extra_headers, compact_request_timeout)
            .await
            .map_err(map_api_error);
        trace_attempt.record_result(result.as_deref());
        result
    }

    pub(crate) async fn create_realtime_call_with_headers(
        &self,
        sdp: String,
        session_config: ApiRealtimeSessionConfig,
        mut extra_headers: ApiHeaderMap,
    ) -> Result<RealtimeWebrtcCallStart> {
        // Create the media call over HTTP first, then retain matching auth so realtime can attach
        // the server-side control WebSocket to the call id from that HTTP response.
        let client_setup = self.current_client_setup().await?;
        if let Some(header_value) = self.generate_attestation_header_for().await {
            extra_headers.insert(X_OAI_ATTESTATION_HEADER, header_value);
        }
        let mut sideband_headers = extra_headers.clone();
        sideband_headers.extend(sideband_websocket_auth_headers(
            client_setup.api_auth.as_ref(),
        ));
        let transport = ReqwestTransport::new(build_reqwest_client());
        let response =
            ApiRealtimeCallClient::new(transport, client_setup.api_provider, client_setup.api_auth)
                .create_with_session_and_headers(sdp, session_config, extra_headers)
                .await
                .map_err(map_api_error)?;
        Ok(RealtimeWebrtcCallStart {
            sdp: response.sdp,
            call_id: response.call_id,
            sideband_headers,
        })
    }

    /// Builds memory summaries for each provided normalized raw memory.
    ///
    /// This is a unary call (no streaming) to `/v1/memories/trace_summarize`.
    ///
    /// The model selection, reasoning effort, and telemetry context are passed explicitly to keep
    /// `ModelClient` session-scoped.
    pub async fn summarize_memories(
        &self,
        raw_memories: Vec<ApiRawMemory>,
        model_info: &ModelInfo,
        effort: Option<ReasoningEffortConfig>,
        session_telemetry: &SessionTelemetry,
    ) -> Result<Vec<ApiMemorySummarizeOutput>> {
        if raw_memories.is_empty() {
            return Ok(Vec::new());
        }

        let client_setup = self.current_client_setup().await?;
        let transport = ReqwestTransport::new(build_reqwest_client());
        let request_telemetry = Self::build_request_telemetry(
            session_telemetry,
            AuthRequestTelemetryContext::new(
                client_setup.auth.as_ref().map(CodexAuth::auth_mode),
                client_setup.api_auth.as_ref(),
                PendingUnauthorizedRetry::default(),
            ),
            RequestRouteTelemetry::for_endpoint(MEMORIES_SUMMARIZE_ENDPOINT),
            self.state.auth_env_telemetry.clone(),
        );
        let client =
            ApiMemoriesClient::new(transport, client_setup.api_provider, client_setup.api_auth)
                .with_telemetry(Some(request_telemetry));

        let payload = ApiMemorySummarizeInput {
            model: model_info.slug.clone(),
            raw_memories,
            reasoning: effort.map(|effort| Reasoning {
                effort: Some(effort),
                summary: None,
            }),
        };

        client
            .summarize_input(&payload, self.build_subagent_headers())
            .await
            .map_err(map_api_error)
    }

    fn build_subagent_headers(&self) -> ApiHeaderMap {
        let mut extra_headers = ApiHeaderMap::new();
        if let Some(subagent) = subagent_header_value(&self.state.session_source)
            && let Ok(val) = HeaderValue::from_str(&subagent)
        {
            extra_headers.insert(X_OPENAI_SUBAGENT_HEADER, val);
        }
        if matches!(
            self.state.session_source,
            SessionSource::Internal(InternalSessionSource::MemoryConsolidation)
        ) {
            extra_headers.insert(
                X_OPENAI_MEMGEN_REQUEST_HEADER,
                HeaderValue::from_static("true"),
            );
        }
        extra_headers
    }

    fn build_responses_identity_headers(&self) -> ApiHeaderMap {
        let mut extra_headers = self.build_subagent_headers();
        if let Some(parent_thread_id) = parent_thread_id_header_value(&self.state.session_source)
            && let Ok(val) = HeaderValue::from_str(&parent_thread_id)
        {
            extra_headers.insert(X_CODEX_PARENT_THREAD_ID_HEADER, val);
        }
        if let Ok(val) = HeaderValue::from_str(&self.current_window_id()) {
            extra_headers.insert(X_CODEX_WINDOW_ID_HEADER, val);
        }
        extra_headers
    }

    fn build_ws_client_metadata(
        &self,
        turn_metadata_header: Option<&str>,
    ) -> HashMap<String, String> {
        let mut client_metadata = HashMap::new();
        client_metadata.insert(
            X_CODEX_INSTALLATION_ID_HEADER.to_string(),
            self.state.installation_id.clone(),
        );
        client_metadata.insert(
            X_CODEX_WINDOW_ID_HEADER.to_string(),
            self.current_window_id(),
        );
        if let Some(subagent) = subagent_header_value(&self.state.session_source) {
            client_metadata.insert(X_OPENAI_SUBAGENT_HEADER.to_string(), subagent);
        }
        if let Some(parent_thread_id) = parent_thread_id_header_value(&self.state.session_source) {
            client_metadata.insert(
                X_CODEX_PARENT_THREAD_ID_HEADER.to_string(),
                parent_thread_id,
            );
        }
        if let Some(turn_metadata_header) = parse_turn_metadata_header(turn_metadata_header)
            && let Ok(turn_metadata) = turn_metadata_header.to_str()
        {
            client_metadata.insert(
                X_CODEX_TURN_METADATA_HEADER.to_string(),
                turn_metadata.to_string(),
            );
        }
        client_metadata
    }

    async fn generate_attestation_header_for(&self) -> Option<HeaderValue> {
        if !self.state.include_attestation {
            return None;
        }

        self.state
            .attestation_provider
            .as_ref()?
            .header_for_request(AttestationContext {
                thread_id: self.state.thread_id,
            })
            .await
    }

    /// Builds request telemetry for unary API calls (e.g., Compact endpoint).
    fn build_request_telemetry(
        session_telemetry: &SessionTelemetry,
        auth_context: AuthRequestTelemetryContext,
        request_route_telemetry: RequestRouteTelemetry,
        auth_env_telemetry: AuthEnvTelemetry,
    ) -> Arc<dyn RequestTelemetry> {
        let telemetry = Arc::new(ApiTelemetry::new(
            session_telemetry.clone(),
            auth_context,
            request_route_telemetry,
            auth_env_telemetry,
        ));
        let request_telemetry: Arc<dyn RequestTelemetry> = telemetry;
        request_telemetry
    }

    fn build_reasoning(
        model_info: &ModelInfo,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
    ) -> Option<Reasoning> {
        if model_info.supports_reasoning_summaries {
            Some(Reasoning {
                effort: effort.or(model_info.default_reasoning_level),
                summary: if summary == ReasoningSummaryConfig::None {
                    None
                } else {
                    Some(summary)
                },
            })
        } else {
            None
        }
    }

    fn build_responses_request(
        &self,
        provider: &codex_api::Provider,
        prompt: &Prompt,
        model_info: &ModelInfo,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
    ) -> Result<ResponsesApiRequest> {
        let instructions = &prompt.base_instructions.text;
        let input = prompt.get_formatted_input();
        let tools = create_tools_json_for_responses_api(&prompt.tools)?;
        let reasoning = Self::build_reasoning(model_info, effort, summary);
        let include = if reasoning.is_some() {
            vec!["reasoning.encrypted_content".to_string()]
        } else {
            Vec::new()
        };
        let verbosity = if model_info.support_verbosity {
            self.state.model_verbosity.or(model_info.default_verbosity)
        } else {
            if self.state.model_verbosity.is_some() {
                warn!(
                    "model_verbosity is set but ignored as the model does not support verbosity: {}",
                    model_info.slug
                );
            }
            None
        };
        let text = create_text_param_for_request(
            verbosity,
            &prompt.output_schema,
            prompt.output_schema_strict,
        );
        let prompt_cache_key = Some(self.state.thread_id.to_string());
        let service_tier = model_info.service_tier_for_request(service_tier);
        let request = ResponsesApiRequest {
            model: model_info.slug.clone(),
            instructions: instructions.clone(),
            input,
            tools,
            tool_choice: "auto".to_string(),
            parallel_tool_calls: prompt.parallel_tool_calls,
            reasoning,
            store: provider.is_azure_responses_endpoint(),
            stream: true,
            include,
            service_tier,
            prompt_cache_key,
            text,
            client_metadata: Some(HashMap::from([(
                X_CODEX_INSTALLATION_ID_HEADER.to_string(),
                self.state.installation_id.clone(),
            )])),
        };
        Ok(request)
    }

    /// Returns whether the Responses-over-WebSocket transport is active for this session.
    ///
    /// WebSocket use is controlled by provider capability and session-scoped fallback state.
    pub fn responses_websocket_enabled(&self) -> bool {
        if !self.state.provider.info().supports_websockets
            || self.state.disable_websockets.load(Ordering::Relaxed)
        {
            return false;
        }

        true
    }

    /// Returns auth + provider configuration resolved from the current session auth state.
    ///
    /// This centralizes setup used by both prewarm and normal request paths so they stay in
    /// lockstep when auth/provider resolution changes.
    async fn current_client_setup(&self) -> Result<CurrentClientSetup> {
        let auth = self.state.provider.auth().await;
        let api_provider = self.state.provider.api_provider().await?;
        let api_auth = self.state.provider.api_auth().await?;
        Ok(CurrentClientSetup {
            auth,
            api_provider,
            api_auth,
        })
    }

    /// Opens a websocket connection using the same header and telemetry wiring as normal turns.
    ///
    /// Both startup prewarm and in-turn `needs_new` reconnects call this path so handshake
    /// behavior remains consistent across both flows.
    #[allow(clippy::too_many_arguments)]
    async fn connect_websocket(
        &self,
        session_telemetry: &SessionTelemetry,
        api_provider: codex_api::Provider,
        api_auth: SharedAuthProvider,
        turn_state: Option<Arc<OnceLock<String>>>,
        turn_metadata_header: Option<&str>,
        auth_context: AuthRequestTelemetryContext,
        request_route_telemetry: RequestRouteTelemetry,
    ) -> std::result::Result<ApiWebSocketConnection, ApiError> {
        let headers = self
            .build_websocket_headers(turn_state.as_ref(), turn_metadata_header)
            .await;
        let websocket_telemetry = ModelClientSession::build_websocket_telemetry(
            session_telemetry,
            auth_context,
            request_route_telemetry,
            self.state.auth_env_telemetry.clone(),
        );
        let websocket_connect_timeout = self.state.provider.info().websocket_connect_timeout();
        let start = Instant::now();
        let result = match tokio::time::timeout(
            websocket_connect_timeout,
            ApiWebSocketResponsesClient::new(api_provider, api_auth).connect(
                headers,
                codex_login::default_client::default_headers(),
                turn_state,
                Some(websocket_telemetry),
            ),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(ApiError::Transport(TransportError::Timeout)),
        };
        let error_message = result.as_ref().err().map(telemetry_api_error_message);
        let response_debug = result
            .as_ref()
            .err()
            .map(extract_response_debug_context_from_api_error)
            .unwrap_or_default();
        let status = result.as_ref().err().and_then(api_error_http_status);
        session_telemetry.record_websocket_connect(
            start.elapsed(),
            status,
            error_message.as_deref(),
            auth_context.auth_header_attached,
            auth_context.auth_header_name,
            auth_context.retry_after_unauthorized,
            auth_context.recovery_mode,
            auth_context.recovery_phase,
            request_route_telemetry.endpoint,
            /*connection_reused*/ false,
            response_debug.request_id.as_deref(),
            response_debug.cf_ray.as_deref(),
            response_debug.auth_error.as_deref(),
            response_debug.auth_error_code.as_deref(),
        );
        emit_feedback_request_tags_with_auth_env(
            &FeedbackRequestTags {
                endpoint: request_route_telemetry.endpoint,
                auth_header_attached: auth_context.auth_header_attached,
                auth_header_name: auth_context.auth_header_name,
                auth_mode: auth_context.auth_mode,
                auth_retry_after_unauthorized: Some(auth_context.retry_after_unauthorized),
                auth_recovery_mode: auth_context.recovery_mode,
                auth_recovery_phase: auth_context.recovery_phase,
                auth_connection_reused: Some(false),
                auth_request_id: response_debug.request_id.as_deref(),
                auth_cf_ray: response_debug.cf_ray.as_deref(),
                auth_error: response_debug.auth_error.as_deref(),
                auth_error_code: response_debug.auth_error_code.as_deref(),
                auth_recovery_followup_success: auth_context
                    .retry_after_unauthorized
                    .then_some(result.is_ok()),
                auth_recovery_followup_status: auth_context
                    .retry_after_unauthorized
                    .then_some(status)
                    .flatten(),
            },
            &self.state.auth_env_telemetry,
        );
        result
    }

    /// Builds websocket handshake headers for both prewarm and turn-time reconnect.
    ///
    /// Callers should pass the current turn-state lock when available so sticky-routing state is
    /// replayed on reconnect within the same turn.
    async fn build_websocket_headers(
        &self,
        turn_state: Option<&Arc<OnceLock<String>>>,
        turn_metadata_header: Option<&str>,
    ) -> ApiHeaderMap {
        let turn_metadata_header = parse_turn_metadata_header(turn_metadata_header);
        let session_id = self.state.session_id.to_string();
        let thread_id = self.state.thread_id.to_string();
        let mut headers = build_responses_headers(
            self.state.beta_features_header.as_deref(),
            turn_state,
            turn_metadata_header.as_ref(),
        );
        if let Ok(header_value) = HeaderValue::from_str(&thread_id) {
            headers.insert("x-client-request-id", header_value);
        }
        headers.extend(build_session_headers(Some(session_id), Some(thread_id)));
        headers.extend(self.build_responses_identity_headers());
        if let Some(header_value) = self.generate_attestation_header_for().await {
            headers.insert(X_OAI_ATTESTATION_HEADER, header_value);
        }
        headers.insert(
            OPENAI_BETA_HEADER,
            HeaderValue::from_static(RESPONSES_WEBSOCKETS_V2_BETA_HEADER_VALUE),
        );
        if self.state.include_timing_metrics {
            headers.insert(
                X_RESPONSESAPI_INCLUDE_TIMING_METRICS_HEADER,
                HeaderValue::from_static("true"),
            );
        }
        headers
    }
}

impl Drop for ModelClientSession {
    fn drop(&mut self) {
        let websocket_session = std::mem::take(&mut self.websocket_session);
        self.client
            .store_cached_websocket_session(websocket_session);
    }
}

impl ModelClientSession {
    fn reset_websocket_session(&mut self) {
        self.websocket_session.connection = None;
        self.websocket_session.last_request = None;
        self.websocket_session.last_response_rx = None;
        self.websocket_session.last_response_from_untraced_warmup = false;
        self.websocket_session
            .set_connection_reused(/*connection_reused*/ false);
    }

    pub(crate) async fn send_response_processed(&self, response_id: &str) {
        let Some(connection) = self.websocket_session.connection.as_ref() else {
            return;
        };
        if let Err(err) = connection
            .send_response_processed(response_id.to_string())
            .await
        {
            debug!("failed to send response.processed websocket request: {err}");
        }
    }

    #[allow(clippy::too_many_arguments)]
    /// Builds shared Responses API transport options and request-body options.
    ///
    /// Keeping option construction in one place ensures request-scoped headers are consistent
    /// regardless of transport choice.
    async fn build_responses_options(
        &self,
        turn_metadata_header: Option<&str>,
        compression: Compression,
    ) -> ApiResponsesOptions {
        let turn_metadata_header = parse_turn_metadata_header(turn_metadata_header);
        let session_id = self.client.state.session_id.to_string();
        let thread_id = self.client.state.thread_id.to_string();
        ApiResponsesOptions {
            session_id: Some(session_id),
            thread_id: Some(thread_id),
            session_source: Some(self.client.state.session_source.clone()),
            extra_headers: {
                let mut headers = build_responses_headers(
                    self.client.state.beta_features_header.as_deref(),
                    Some(&self.turn_state),
                    turn_metadata_header.as_ref(),
                );
                headers.extend(self.client.build_responses_identity_headers());
                if let Some(header_value) = self.client.generate_attestation_header_for().await {
                    headers.insert(X_OAI_ATTESTATION_HEADER, header_value);
                }
                headers
            },
            compression,
            turn_state: Some(Arc::clone(&self.turn_state)),
        }
    }

    fn get_incremental_items(
        &self,
        request: &ResponsesApiRequest,
        last_response: Option<&LastResponse>,
        allow_empty_delta: bool,
    ) -> Option<Vec<ResponseItem>> {
        // Checks whether the current request is an incremental extension of the previous request.
        // We only reuse an incremental input delta when non-input request fields are unchanged and
        // `input` is a strict
        // extension of the previous known input. Server-returned output items are treated as part
        // of the baseline so we do not resend them.
        let previous_request = self.websocket_session.last_request.as_ref()?;
        let mut previous_without_input = previous_request.clone();
        previous_without_input.input.clear();
        let mut request_without_input = request.clone();
        request_without_input.input.clear();
        if previous_without_input != request_without_input {
            trace!(
                "incremental request failed, properties didn't match {previous_without_input:?} != {request_without_input:?}"
            );
            return None;
        }

        let mut baseline = previous_request.input.clone();
        if let Some(last_response) = last_response {
            baseline.extend(last_response.items_added.clone());
        }

        let baseline_len = baseline.len();
        if request.input.starts_with(&baseline)
            && (allow_empty_delta || baseline_len < request.input.len())
        {
            Some(request.input[baseline_len..].to_vec())
        } else {
            trace!("incremental request failed, items didn't match");
            None
        }
    }

    fn get_last_response(&mut self) -> Option<LastResponse> {
        self.websocket_session
            .last_response_rx
            .take()
            .and_then(|mut receiver| match receiver.try_recv() {
                Ok(last_response) => Some(last_response),
                Err(TryRecvError::Closed) | Err(TryRecvError::Empty) => None,
            })
    }

    fn prepare_websocket_request(
        &mut self,
        payload: ResponseCreateWsRequest,
        request: &ResponsesApiRequest,
    ) -> (ResponsesWsRequest, bool) {
        let Some(last_response) = self.get_last_response() else {
            return (ResponsesWsRequest::ResponseCreate(payload), false);
        };
        let previous_response_id_from_untraced_warmup =
            self.websocket_session.last_response_from_untraced_warmup;
        let Some(incremental_items) = self.get_incremental_items(
            request,
            Some(&last_response),
            /*allow_empty_delta*/ true,
        ) else {
            return (ResponsesWsRequest::ResponseCreate(payload), false);
        };

        if last_response.response_id.is_empty() {
            trace!("incremental request failed, no previous response id");
            return (ResponsesWsRequest::ResponseCreate(payload), false);
        }

        (
            ResponsesWsRequest::ResponseCreate(ResponseCreateWsRequest {
                previous_response_id: Some(last_response.response_id),
                input: incremental_items,
                ..payload
            }),
            previous_response_id_from_untraced_warmup,
        )
    }

    /// Opportunistically preconnects a websocket for this turn-scoped client session.
    ///
    /// This performs only connection setup; it never sends prompt payloads.
    pub async fn preconnect_websocket(
        &mut self,
        session_telemetry: &SessionTelemetry,
        _model_info: &ModelInfo,
    ) -> std::result::Result<(), ApiError> {
        if !self.client.responses_websocket_enabled() {
            return Ok(());
        }
        if self.websocket_session.connection.is_some() {
            return Ok(());
        }

        let client_setup = self.client.current_client_setup().await.map_err(|err| {
            ApiError::Stream(format!(
                "failed to build websocket prewarm client setup: {err}"
            ))
        })?;
        let auth_context = AuthRequestTelemetryContext::new(
            client_setup.auth.as_ref().map(CodexAuth::auth_mode),
            client_setup.api_auth.as_ref(),
            PendingUnauthorizedRetry::default(),
        );
        let connection = self
            .client
            .connect_websocket(
                session_telemetry,
                client_setup.api_provider,
                client_setup.api_auth,
                Some(Arc::clone(&self.turn_state)),
                /*turn_metadata_header*/ None,
                auth_context,
                RequestRouteTelemetry::for_endpoint(RESPONSES_ENDPOINT),
            )
            .await?;
        self.websocket_session.connection = Some(connection);
        self.websocket_session
            .set_connection_reused(/*connection_reused*/ false);
        Ok(())
    }
    /// Returns a websocket connection for this turn.
    #[instrument(
        name = "model_client.websocket_connection",
        level = "info",
        skip_all,
        fields(
            provider = %self.client.state.provider.info().name,
            wire_api = %self.client.state.provider.info().wire_api,
            transport = "responses_websocket",
            api.path = "responses",
            turn.has_metadata_header = params.turn_metadata_header.is_some()
        )
    )]
    async fn websocket_connection(
        &mut self,
        params: WebsocketConnectParams<'_>,
    ) -> std::result::Result<&ApiWebSocketConnection, ApiError> {
        let WebsocketConnectParams {
            session_telemetry,
            api_provider,
            api_auth,
            turn_metadata_header,
            options,
            auth_context,
            request_route_telemetry,
        } = params;
        let needs_new = match self.websocket_session.connection.as_ref() {
            Some(conn) => conn.is_closed().await,
            None => true,
        };

        if needs_new {
            self.websocket_session.last_request = None;
            self.websocket_session.last_response_rx = None;
            self.websocket_session.last_response_from_untraced_warmup = false;
            let turn_state = options
                .turn_state
                .clone()
                .unwrap_or_else(|| Arc::clone(&self.turn_state));
            let new_conn = match self
                .client
                .connect_websocket(
                    session_telemetry,
                    api_provider,
                    api_auth,
                    Some(turn_state),
                    turn_metadata_header,
                    auth_context,
                    request_route_telemetry,
                )
                .await
            {
                Ok(new_conn) => new_conn,
                Err(err) => {
                    if matches!(err, ApiError::Transport(TransportError::Timeout)) {
                        self.reset_websocket_session();
                    }
                    return Err(err);
                }
            };
            self.websocket_session.connection = Some(new_conn);
            self.websocket_session
                .set_connection_reused(/*connection_reused*/ false);
        } else {
            self.websocket_session
                .set_connection_reused(/*connection_reused*/ true);
        }

        self.websocket_session
            .connection
            .as_ref()
            .ok_or(ApiError::Stream(
                "websocket connection is unavailable".to_string(),
            ))
    }

    fn responses_request_compression(&self, auth: Option<&CodexAuth>) -> Compression {
        if self.client.state.enable_request_compression
            && auth.is_some_and(CodexAuth::uses_codex_backend)
            && self.client.state.provider.info().is_openai()
        {
            Compression::Zstd
        } else {
            Compression::None
        }
    }

    /// Streams a turn via the OpenAI Responses API.
    ///
    /// Handles reasoning summaries, verbosity, and the `text` controls used for output schemas.
    #[allow(clippy::too_many_arguments)]
    #[instrument(
        name = "model_client.stream_responses_api",
        level = "info",
        skip_all,
        fields(
            model = %model_info.slug,
            wire_api = %self.client.state.provider.info().wire_api,
            transport = "responses_http",
            http.method = "POST",
            api.path = "responses",
            turn.has_metadata_header = turn_metadata_header.is_some()
        )
    )]
    async fn stream_responses_api(
        &self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        session_telemetry: &SessionTelemetry,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        turn_metadata_header: Option<&str>,
        inference_trace: &InferenceTraceContext,
    ) -> Result<ResponseStream> {
        let auth_manager = self.client.state.provider.auth_manager();
        let mut auth_recovery = auth_manager
            .as_ref()
            .map(AuthManager::unauthorized_recovery);
        let mut pending_retry = PendingUnauthorizedRetry::default();
        loop {
            let client_setup = self.client.current_client_setup().await?;
            let transport = ReqwestTransport::new(build_reqwest_client());
            let request_auth_context = AuthRequestTelemetryContext::new(
                client_setup.auth.as_ref().map(CodexAuth::auth_mode),
                client_setup.api_auth.as_ref(),
                pending_retry,
            );
            let (request_telemetry, sse_telemetry) = Self::build_streaming_telemetry(
                session_telemetry,
                request_auth_context,
                RequestRouteTelemetry::for_endpoint(RESPONSES_ENDPOINT),
                self.client.state.auth_env_telemetry.clone(),
            );
            let compression = self.responses_request_compression(client_setup.auth.as_ref());
            let mut options = self
                .build_responses_options(turn_metadata_header, compression)
                .await;

            let request = self.client.build_responses_request(
                &client_setup.api_provider,
                prompt,
                model_info,
                effort,
                summary,
                service_tier.clone(),
            )?;
            let inference_trace_attempt = inference_trace.start_attempt();
            inference_trace_attempt.add_request_headers(&mut options.extra_headers);
            inference_trace_attempt.record_started(&request);
            let client = ApiResponsesClient::new(
                transport,
                client_setup.api_provider,
                client_setup.api_auth,
            )
            .with_telemetry(Some(request_telemetry), Some(sse_telemetry));
            let stream_result = client.stream_request(request, options).await;

            match stream_result {
                Ok(stream) => {
                    let (stream, _) = map_response_stream(
                        stream,
                        session_telemetry.clone(),
                        inference_trace_attempt,
                    );
                    return Ok(stream);
                }
                Err(ApiError::Transport(
                    unauthorized_transport @ TransportError::Http { status, .. },
                )) if status == StatusCode::UNAUTHORIZED => {
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
                            session_telemetry,
                        )
                        .await?,
                    );
                    continue;
                }
                Err(err) => {
                    let response_debug_context =
                        extract_response_debug_context_from_api_error(&err);
                    let err = map_api_error(err);
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

    /// Streams a turn via the Responses API over WebSocket transport.
    #[allow(clippy::too_many_arguments)]
    #[instrument(
        name = "model_client.stream_responses_websocket",
        level = "info",
        skip_all,
        fields(
            model = %model_info.slug,
            wire_api = %self.client.state.provider.info().wire_api,
            transport = "responses_websocket",
            api.path = "responses",
            turn.has_metadata_header = turn_metadata_header.is_some(),
            websocket.warmup = warmup
        )
    )]
    async fn stream_responses_websocket(
        &mut self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        session_telemetry: &SessionTelemetry,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        turn_metadata_header: Option<&str>,
        warmup: bool,
        request_trace: Option<W3cTraceContext>,
        inference_trace: &InferenceTraceContext,
    ) -> Result<WebsocketStreamOutcome> {
        let auth_manager = self.client.state.provider.auth_manager();

        let mut auth_recovery = auth_manager
            .as_ref()
            .map(AuthManager::unauthorized_recovery);
        let mut pending_retry = PendingUnauthorizedRetry::default();
        loop {
            let client_setup = self.client.current_client_setup().await?;
            let request_auth_context = AuthRequestTelemetryContext::new(
                client_setup.auth.as_ref().map(CodexAuth::auth_mode),
                client_setup.api_auth.as_ref(),
                pending_retry,
            );
            let compression = self.responses_request_compression(client_setup.auth.as_ref());

            let options = self
                .build_responses_options(turn_metadata_header, compression)
                .await;
            let request = self.client.build_responses_request(
                &client_setup.api_provider,
                prompt,
                model_info,
                effort,
                summary,
                service_tier.clone(),
            )?;
            let mut ws_payload = ResponseCreateWsRequest {
                client_metadata: response_create_client_metadata(
                    Some(self.client.build_ws_client_metadata(turn_metadata_header)),
                    request_trace.as_ref(),
                ),
                ..ResponseCreateWsRequest::from(&request)
            };
            if warmup {
                ws_payload.generate = Some(false);
            }

            match self
                .websocket_connection(WebsocketConnectParams {
                    session_telemetry,
                    api_provider: client_setup.api_provider,
                    api_auth: client_setup.api_auth,
                    turn_metadata_header,
                    options: &options,
                    auth_context: request_auth_context,
                    request_route_telemetry: RequestRouteTelemetry::for_endpoint(
                        RESPONSES_ENDPOINT,
                    ),
                })
                .await
            {
                Ok(_) => {}
                Err(ApiError::Transport(TransportError::Http { status, .. }))
                    if status == StatusCode::UPGRADE_REQUIRED =>
                {
                    return Ok(WebsocketStreamOutcome::FallbackToHttp);
                }
                Err(ApiError::Transport(
                    unauthorized_transport @ TransportError::Http { status, .. },
                )) if status == StatusCode::UNAUTHORIZED => {
                    pending_retry = PendingUnauthorizedRetry::from_recovery(
                        handle_unauthorized(
                            unauthorized_transport,
                            &mut auth_recovery,
                            session_telemetry,
                        )
                        .await?,
                    );
                    continue;
                }
                Err(err) => return Err(map_api_error(err)),
            }

            let (mut ws_request, previous_response_id_from_untraced_warmup) =
                self.prepare_websocket_request(ws_payload, &request);
            let inference_trace_attempt = if warmup {
                // Prewarm sends `generate=false`; it is connection setup, not a
                // model inference attempt that should appear in rollout traces.
                InferenceTraceAttempt::disabled()
            } else {
                inference_trace.start_attempt()
            };
            stamp_ws_stream_request_start_ms(&mut ws_request);
            if previous_response_id_from_untraced_warmup {
                // The transport can reuse an untraced warmup response id and omit the
                // already-sent input, but rollout replay needs the logical model-visible
                // request rather than the compressed websocket delta.
                inference_trace_attempt.record_started(&request);
            } else {
                inference_trace_attempt.record_started(&ws_request);
            }
            self.websocket_session.last_request = Some(request);
            self.websocket_session.last_response_from_untraced_warmup = warmup;
            let websocket_connection =
                self.websocket_session.connection.as_ref().ok_or_else(|| {
                    map_api_error(ApiError::Stream(
                        "websocket connection is unavailable".to_string(),
                    ))
                })?;
            let stream_result = websocket_connection
                .stream_request(ws_request, self.websocket_session.connection_reused())
                .await
                .map_err(|err| {
                    let response_debug_context =
                        extract_response_debug_context_from_api_error(&err);
                    let err = map_api_error(err);
                    inference_trace_attempt.record_failed(
                        &err,
                        response_debug_context.request_id.as_deref(),
                        /*output_items*/ &[],
                    );
                    err
                })?;
            let (stream, last_request_rx) = map_response_stream(
                stream_result,
                session_telemetry.clone(),
                inference_trace_attempt,
            );
            self.websocket_session.last_response_rx = Some(last_request_rx);
            return Ok(WebsocketStreamOutcome::Stream(stream));
        }
    }

    /// Builds request and SSE telemetry for streaming API calls.
    fn build_streaming_telemetry(
        session_telemetry: &SessionTelemetry,
        auth_context: AuthRequestTelemetryContext,
        request_route_telemetry: RequestRouteTelemetry,
        auth_env_telemetry: AuthEnvTelemetry,
    ) -> (Arc<dyn RequestTelemetry>, Arc<dyn SseTelemetry>) {
        let telemetry = Arc::new(ApiTelemetry::new(
            session_telemetry.clone(),
            auth_context,
            request_route_telemetry,
            auth_env_telemetry,
        ));
        let request_telemetry: Arc<dyn RequestTelemetry> = telemetry.clone();
        let sse_telemetry: Arc<dyn SseTelemetry> = telemetry;
        (request_telemetry, sse_telemetry)
    }

    /// Builds telemetry for the Responses API WebSocket transport.
    fn build_websocket_telemetry(
        session_telemetry: &SessionTelemetry,
        auth_context: AuthRequestTelemetryContext,
        request_route_telemetry: RequestRouteTelemetry,
        auth_env_telemetry: AuthEnvTelemetry,
    ) -> Arc<dyn WebsocketTelemetry> {
        let telemetry = Arc::new(ApiTelemetry::new(
            session_telemetry.clone(),
            auth_context,
            request_route_telemetry,
            auth_env_telemetry,
        ));
        let websocket_telemetry: Arc<dyn WebsocketTelemetry> = telemetry;
        websocket_telemetry
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn prewarm_websocket(
        &mut self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        session_telemetry: &SessionTelemetry,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        turn_metadata_header: Option<&str>,
    ) -> Result<()> {
        if !self.client.responses_websocket_enabled() {
            return Ok(());
        }
        if self.websocket_session.last_request.is_some() {
            return Ok(());
        }

        let disabled_trace = InferenceTraceContext::disabled();
        match self
            .stream_responses_websocket(
                prompt,
                model_info,
                session_telemetry,
                effort,
                summary,
                service_tier,
                turn_metadata_header,
                /*warmup*/ true,
                current_span_w3c_trace_context(),
                &disabled_trace,
            )
            .await
        {
            Ok(WebsocketStreamOutcome::Stream(mut stream)) => {
                // Wait for the v2 warmup request to complete before sending the first turn request.
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(ResponseEvent::Completed { .. }) => break,
                        Err(err) => return Err(err),
                        _ => {}
                    }
                }
                Ok(())
            }
            Ok(WebsocketStreamOutcome::FallbackToHttp) => {
                self.try_switch_fallback_transport(session_telemetry, model_info);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    #[allow(clippy::too_many_arguments)]
    /// Streams a single model request within the current turn.
    ///
    /// The caller is responsible for passing per-turn settings explicitly (model selection,
    /// reasoning settings, telemetry context, and turn metadata). This method will prefer the
    /// Responses WebSocket transport when the provider supports it and it remains healthy, and will
    /// fall back to the HTTP Responses API transport otherwise. The trace context may be enabled or
    /// disabled, but is always explicit so transport paths do not need separate trace/no-trace
    /// branches.
    pub async fn stream(
        &mut self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        session_telemetry: &SessionTelemetry,
        effort: Option<ReasoningEffortConfig>,
        summary: ReasoningSummaryConfig,
        service_tier: Option<String>,
        turn_metadata_header: Option<&str>,
        inference_trace: &InferenceTraceContext,
    ) -> Result<ResponseStream> {
        let wire_api = self.client.state.provider.info().wire_api;
        match wire_api {
            WireApi::Responses => {
                if self.client.responses_websocket_enabled() {
                    let request_trace = current_span_w3c_trace_context();
                    match self
                        .stream_responses_websocket(
                            prompt,
                            model_info,
                            session_telemetry,
                            effort,
                            summary,
                            service_tier.clone(),
                            turn_metadata_header,
                            /*warmup*/ false,
                            request_trace,
                            inference_trace,
                        )
                        .await?
                    {
                        WebsocketStreamOutcome::Stream(stream) => return Ok(stream),
                        WebsocketStreamOutcome::FallbackToHttp => {
                            self.try_switch_fallback_transport(session_telemetry, model_info);
                        }
                    }
                }

                self.stream_responses_api(
                    prompt,
                    model_info,
                    session_telemetry,
                    effort,
                    summary,
                    service_tier,
                    turn_metadata_header,
                    inference_trace,
                )
                .await
            }
            WireApi::Chat => {
                self.stream_chat_completions_api(
                    prompt,
                    model_info,
                    session_telemetry,
                    effort,
                    summary,
                    service_tier,
                    turn_metadata_header,
                    inference_trace,
                )
                .await
            }
            WireApi::Anthropic => {
                self.stream_anthropic_api(prompt, model_info, session_telemetry, inference_trace)
                    .await
            }
        }
    }

    /// Permanently disables WebSockets for this Codex session and resets WebSocket state.
    ///
    /// This is used after exhausting the provider retry budget, to force subsequent requests onto
    /// the HTTP transport.
    ///
    /// Returns `true` if this call activated fallback, or `false` if fallback was already active.
    pub(crate) fn try_switch_fallback_transport(
        &mut self,
        session_telemetry: &SessionTelemetry,
        model_info: &ModelInfo,
    ) -> bool {
        let activated = self
            .client
            .force_http_fallback(session_telemetry, model_info);
        self.websocket_session = WebsocketSession::default();
        activated
    }

    /// Streams a turn via the OpenAI Chat Completions API.
    #[allow(clippy::too_many_arguments)]
    #[instrument(
        name = "model_client.stream_chat_completions_api",
        level = "info",
        skip_all,
        fields(
            model = %model_info.slug,
            wire_api = %self.client.state.provider.info().wire_api,
            transport = "chat_completions_http",
            http.method = "POST",
            api.path = "chat/completions"
        )
    )]
    async fn stream_chat_completions_api(
        &self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        session_telemetry: &SessionTelemetry,
        effort: Option<ReasoningEffortConfig>,
        _summary: ReasoningSummaryConfig,
        _service_tier: Option<ServiceTier>,
        _turn_metadata_header: Option<&str>,
        inference_trace: &InferenceTraceContext,
    ) -> Result<ResponseStream> {
        let auth_manager = self.client.state.provider.auth_manager();
        let mut auth_recovery = auth_manager
            .as_ref()
            .map(AuthManager::unauthorized_recovery);
        let mut pending_retry = PendingUnauthorizedRetry::default();
        let mut retry_count: u32 = 0;
        let mut auth_retry_count: u32 = 0;
        const MAX_AUTH_RETRIES: u32 = 10;
        let base_delay = Duration::from_secs(5);
        let max_delay = Duration::from_secs(600); // 10 minutes
        loop {
            let client_setup = self.client.current_client_setup().await?;
            let transport = ReqwestTransport::new(build_reqwest_client());
            let request_auth_context = AuthRequestTelemetryContext::new(
                client_setup.auth.as_ref().map(CodexAuth::auth_mode),
                client_setup.api_auth.as_ref(),
                pending_retry,
            );
            let (_, _sse_telemetry) = Self::build_streaming_telemetry(
                session_telemetry,
                request_auth_context,
                RequestRouteTelemetry::for_endpoint("/chat/completions"),
                self.client.state.auth_env_telemetry.clone(),
            );

            let request = self.build_chat_completions_request(prompt, model_info, effort)?;
            let inference_trace_attempt = inference_trace.start_attempt();
            inference_trace_attempt.record_started(&request);
            let chat_stream = self.client.state.provider.info().chat_stream;
            let client = ApiChatCompletionsClient::new(
                transport,
                client_setup.api_provider,
                client_setup.api_auth,
                chat_stream,
            );
            let stream_result = client.request(request, ApiHeaderMap::new()).await;

            match stream_result {
                Ok(stream) => {
                    let (stream, _) = map_response_stream(
                        stream,
                        session_telemetry.clone(),
                        inference_trace_attempt,
                    );
                    return Ok(stream);
                }
                Err(ApiError::Transport(
                    unauthorized_transport @ TransportError::Http { status, .. },
                )) if status == StatusCode::UNAUTHORIZED => {
                    inference_trace_attempt.record_failed(
                        &unauthorized_transport,
                        /*upstream_request_id*/ None,
                        /*output_items*/ &[],
                    );
                    match handle_unauthorized(
                        unauthorized_transport,
                        &mut auth_recovery,
                        session_telemetry,
                    )
                    .await
                    {
                        Ok(recovery) => {
                            pending_retry = PendingUnauthorizedRetry::from_recovery(recovery);
                            retry_count = 0;
                            auth_retry_count = 0;
                            continue;
                        }
                        Err(err) => {
                            // Custom API providers can't refresh tokens, but 401s
                            // may be transient. Retry with backoff up to 10 times.
                            if auth_retry_count < MAX_AUTH_RETRIES {
                                let delay = base_delay
                                    .saturating_mul(
                                        1u32.checked_shl(auth_retry_count.min(20))
                                            .unwrap_or(u32::MAX),
                                    )
                                    .min(max_delay);
                                warn!(
                                    auth_retry_count,
                                    delay_ms = delay.as_millis(),
                                    "Chat Completions received 401, retrying after backoff"
                                );
                                sleep(delay).await;
                                auth_retry_count += 1;
                                continue;
                            }
                            return Err(err);
                        }
                    }
                }
                Err(err) => {
                    inference_trace_attempt.record_failed(
                        &err,
                        /*upstream_request_id*/ None,
                        /*output_items*/ &[],
                    );
                    let delay = match &err {
                        ApiError::Retryable { delay: Some(d), .. } => *d,
                        _ => {
                            // Exponential backoff: 5s, 10s, 20s, 40s, ..., capped at 10 min
                            let multiplier =
                                1u32.checked_shl(retry_count.min(20)).unwrap_or(u32::MAX);
                            let base = base_delay.saturating_mul(multiplier);
                            std::cmp::min(base, max_delay)
                        }
                    };
                    warn!(
                        retry_count,
                        delay_ms = delay.as_millis(),
                        error = %err,
                        "Chat Completions request failed, retrying after backoff"
                    );
                    sleep(delay).await;
                    retry_count += 1;
                    continue;
                }
            }
        }
    }

    /// Streams a turn via the Anthropic Messages API.
    #[instrument(
        name = "model_client.stream_anthropic_api",
        level = "info",
        skip_all,
        fields(
            model = %model_info.slug,
            wire_api = %self.client.state.provider.info().wire_api,
            transport = "anthropic_http",
            http.method = "POST",
            api.path = "v1/messages"
        )
    )]
    async fn stream_anthropic_api(
        &self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        session_telemetry: &SessionTelemetry,
        inference_trace: &InferenceTraceContext,
    ) -> Result<ResponseStream> {
        let auth_manager = self.client.state.provider.auth_manager();
        let mut auth_recovery = auth_manager
            .as_ref()
            .map(AuthManager::unauthorized_recovery);
        let mut pending_retry = PendingUnauthorizedRetry::default();
        let mut retry_count: u32 = 0;
        let mut auth_retry_count: u32 = 0;
        const MAX_AUTH_RETRIES: u32 = 10;
        let base_delay = Duration::from_secs(5);
        let max_delay = Duration::from_secs(600);
        loop {
            let client_setup = self.client.current_client_setup().await?;
            let transport = ReqwestTransport::new(build_reqwest_client());
            let request_auth_context = AuthRequestTelemetryContext::new(
                client_setup.auth.as_ref().map(CodexAuth::auth_mode),
                client_setup.api_auth.as_ref(),
                pending_retry,
            );
            let (_, _sse_telemetry) = Self::build_streaming_telemetry(
                session_telemetry,
                request_auth_context,
                RequestRouteTelemetry::for_endpoint("/v1/messages"),
                self.client.state.auth_env_telemetry.clone(),
            );

            let request = crate::client_anthropic::build_anthropic_request(prompt, model_info)?;
            let inference_trace_attempt = inference_trace.start_attempt();
            inference_trace_attempt.record_started(&request);
            let chat_stream = self.client.state.provider.info().chat_stream;
            let client = ApiAnthropicClient::new(
                transport,
                client_setup.api_provider,
                client_setup.api_auth,
                chat_stream,
            );
            let stream_result = client.request(request, ApiHeaderMap::new()).await;

            match stream_result {
                Ok(stream) => {
                    let (stream, _) = map_response_stream(
                        stream,
                        session_telemetry.clone(),
                        inference_trace_attempt,
                    );
                    return Ok(stream);
                }
                Err(ApiError::Transport(
                    unauthorized_transport @ TransportError::Http { status, .. },
                )) if status == StatusCode::UNAUTHORIZED => {
                    inference_trace_attempt.record_failed(
                        &unauthorized_transport,
                        /*upstream_request_id*/ None,
                        /*output_items*/ &[],
                    );
                    match handle_unauthorized(
                        unauthorized_transport,
                        &mut auth_recovery,
                        session_telemetry,
                    )
                    .await
                    {
                        Ok(recovery) => {
                            pending_retry = PendingUnauthorizedRetry::from_recovery(recovery);
                            retry_count = 0;
                            auth_retry_count = 0;
                            continue;
                        }
                        Err(err) => {
                            if auth_retry_count < MAX_AUTH_RETRIES {
                                let delay = base_delay
                                    .saturating_mul(
                                        1u32.checked_shl(auth_retry_count.min(20))
                                            .unwrap_or(u32::MAX),
                                    )
                                    .min(max_delay);
                                warn!(
                                    auth_retry_count,
                                    delay_ms = delay.as_millis(),
                                    "Anthropic received 401, retrying after backoff"
                                );
                                sleep(delay).await;
                                auth_retry_count += 1;
                                continue;
                            }
                            return Err(err);
                        }
                    }
                }
                Err(err) => {
                    inference_trace_attempt.record_failed(
                        &err,
                        /*upstream_request_id*/ None,
                        /*output_items*/ &[],
                    );
                    let delay = match &err {
                        ApiError::Retryable { delay: Some(d), .. } => *d,
                        _ => {
                            let multiplier =
                                1u32.checked_shl(retry_count.min(20)).unwrap_or(u32::MAX);
                            let base = base_delay.saturating_mul(multiplier);
                            std::cmp::min(base, max_delay)
                        }
                    };
                    warn!(
                        retry_count,
                        delay_ms = delay.as_millis(),
                        error = %err,
                        "Anthropic request failed, retrying after backoff"
                    );
                    sleep(delay).await;
                    retry_count += 1;
                    continue;
                }
            }
        }
    }

    /// Builds a ChatCompletionsRequest from a Prompt.
    fn build_chat_completions_request(
        &self,
        prompt: &Prompt,
        model_info: &ModelInfo,
        effort: Option<ReasoningEffortConfig>,
    ) -> Result<ChatCompletionsRequest> {
        let instructions = &prompt.base_instructions.text;
        let input = prompt.get_formatted_input();

        // Convert instructions to a system message
        let mut messages = Vec::new();
        if !instructions.is_empty() {
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: Some(serde_json::Value::String(instructions.clone())),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                reasoning: None,
            });
        }

        // Convert ResponseItems to ChatMessages
        // First pass: collect reasoning content by anchor index
        let mut reasoning_by_index: HashMap<usize, String> = HashMap::new();
        for (idx, item) in input.iter().enumerate() {
            if let ResponseItem::Reasoning {
                content: Some(items),
                ..
            } = item
            {
                let mut text = String::new();
                for entry in items {
                    match entry {
                        codex_protocol::models::ReasoningItemContent::ReasoningText {
                            text: segment,
                        }
                        | codex_protocol::models::ReasoningItemContent::Text { text: segment } => {
                            text.push_str(segment)
                        }
                    }
                }
                if !text.trim().is_empty() {
                    // Attach reasoning to the next assistant output in this turn.
                    // In thinking mode (e.g., DeepSeek), reasoning precedes the
                    // assistant's content or tool calls, so it should be attached
                    // to the *next* relevant item (Message with role=assistant or
                    // FunctionCall), not a previous assistant message from an
                    // earlier turn.
                    for next_idx in (idx + 1)..input.len() {
                        match &input[next_idx] {
                            ResponseItem::Message { role, .. } if role == "assistant" => {
                                reasoning_by_index
                                    .entry(next_idx)
                                    .and_modify(|v| {
                                        v.push('\n');
                                        v.push_str(&text)
                                    })
                                    .or_insert(text.clone());
                                break;
                            }
                            ResponseItem::FunctionCall { .. } => {
                                reasoning_by_index
                                    .entry(next_idx)
                                    .and_modify(|v| {
                                        v.push('\n');
                                        v.push_str(&text)
                                    })
                                    .or_insert(text.clone());
                                break;
                            }
                            // Stop searching if we hit a non-assistant boundary
                            ResponseItem::Message { role, .. } if role != "assistant" => break,
                            ResponseItem::FunctionCallOutput { .. } => break,
                            ResponseItem::CustomToolCallOutput { .. } => break,
                            _ => {}
                        }
                    }
                }
            }
        }

        // Second pass: build messages with reasoning attached.
        // Consecutive assistant-turn items (Message with role=assistant, FunctionCall,
        // CustomToolCall, Reasoning) must be merged into a single ChatMessage because
        // the Chat Completions API requires all tool_calls and content from one
        // assistant turn to appear on the same message, with tool results immediately
        // following. If we emit separate messages for text and tool_calls, providers
        // that validate message ordering will reject the request with a 400 error.
        let mut pending_assistant: Option<ChatMessage> = None;
        // Warning messages are recorded before the tool result they describe.
        // Queue them until the current assistant tool-result block is complete,
        // or we reach a non-tool boundary.
        let mut pending_warnings: Vec<ChatMessage> = Vec::new();
        let mut pending_tool_outputs_in_turn = 0usize;
        for (idx, item) in input.iter().enumerate() {
            let reasoning = reasoning_by_index.get(&idx).cloned();
            match item {
                ResponseItem::Message { role, content, .. } => {
                    let mapped_role = match role.as_str() {
                        "developer" | "system" => "system",
                        "assistant" => "assistant",
                        "user" => "user",
                        other => other,
                    };
                    let is_assistant = mapped_role == "assistant";
                    let msg_content = content_items_to_chat_content(content, is_assistant);
                    // Text-only view for Warning: detection; images never start with "Warning:".
                    let text_only = content
                        .iter()
                        .filter_map(|c| match c {
                            codex_protocol::models::ContentItem::OutputText { text, .. } => {
                                Some(text.clone())
                            }
                            codex_protocol::models::ContentItem::InputText { text, .. } => {
                                Some(text.clone())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    if mapped_role == "assistant" {
                        if pending_assistant.is_none()
                            && pending_tool_outputs_in_turn == 0
                            && !pending_warnings.is_empty()
                        {
                            messages.append(&mut pending_warnings);
                        }
                        if let Some(msg) = pending_assistant.as_mut() {
                            // Merge content into existing pending assistant message.
                            // This handles the case where FunctionCall items appear
                            // before the Message item in the ResponseItem list.
                            if let Some(c) = msg_content.clone() {
                                msg.content = Some(c);
                            }
                            if msg.reasoning.is_none() && reasoning.is_some() {
                                msg.reasoning_content = reasoning.clone();
                                msg.reasoning = reasoning;
                            }
                        } else {
                            // Start a new pending assistant message.
                            let msg_reasoning = if reasoning.is_some() { reasoning } else { None };
                            pending_assistant = Some(ChatMessage {
                                role: "assistant".to_string(),
                                content: msg_content.clone(),
                                tool_calls: None,
                                tool_call_id: None,
                                reasoning_content: msg_reasoning.clone(),
                                reasoning: msg_reasoning,
                            });
                        }
                    } else {
                        if mapped_role == "user" && text_only.starts_with("Warning:") {
                            pending_warnings.push(ChatMessage {
                                role: "user".to_string(),
                                content: Some(serde_json::Value::String(text_only)),
                                tool_calls: None,
                                tool_call_id: None,
                                reasoning_content: None,
                                reasoning: None,
                            });
                            continue;
                        }
                        if let Some(msg) = pending_assistant.take() {
                            messages.push(msg);
                        }
                        pending_tool_outputs_in_turn = 0;
                        if !pending_warnings.is_empty() {
                            messages.append(&mut pending_warnings);
                        }
                        messages.push(ChatMessage {
                            role: mapped_role.to_string(),
                            content: msg_content,
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning_content: None,
                            reasoning: None,
                        });
                    }
                }
                ResponseItem::FunctionCall {
                    name,
                    arguments,
                    call_id,
                    ..
                } => {
                    let tool_call = codex_api::ChatToolCall {
                        id: call_id.clone(),
                        r#type: "function".to_string(),
                        function: codex_api::ChatFunctionCall {
                            name: name.clone(),
                            arguments: Some(arguments.clone()),
                        },
                    };
                    if pending_assistant.is_none()
                        && pending_tool_outputs_in_turn == 0
                        && !pending_warnings.is_empty()
                    {
                        messages.append(&mut pending_warnings);
                    }
                    match pending_assistant.as_mut() {
                        Some(msg) => {
                            // Append to existing pending assistant message.
                            msg.tool_calls.get_or_insert_with(Vec::new).push(tool_call);
                            if msg.reasoning.is_none() && reasoning.is_some() {
                                msg.reasoning_content = reasoning.clone();
                                msg.reasoning = reasoning;
                            }
                        }
                        None => {
                            // No pending assistant — start one with just this tool call.
                            pending_assistant = Some(ChatMessage {
                                role: "assistant".to_string(),
                                content: None,
                                tool_calls: Some(vec![tool_call]),
                                tool_call_id: None,
                                reasoning_content: reasoning.clone(),
                                reasoning,
                            });
                        }
                    }
                    pending_tool_outputs_in_turn += 1;
                }
                ResponseItem::CustomToolCall {
                    call_id,
                    name,
                    input: tool_input,
                    ..
                } => {
                    let tool_call = codex_api::ChatToolCall {
                        id: call_id.clone(),
                        r#type: "function".to_string(),
                        function: codex_api::ChatFunctionCall {
                            name: name.clone(),
                            arguments: Some(tool_input.clone()),
                        },
                    };
                    if pending_assistant.is_none()
                        && pending_tool_outputs_in_turn == 0
                        && !pending_warnings.is_empty()
                    {
                        messages.append(&mut pending_warnings);
                    }
                    match pending_assistant.as_mut() {
                        Some(msg) => {
                            msg.tool_calls.get_or_insert_with(Vec::new).push(tool_call);
                            if msg.reasoning.is_none() && reasoning.is_some() {
                                msg.reasoning_content = reasoning.clone();
                                msg.reasoning = reasoning;
                            }
                        }
                        None => {
                            pending_assistant = Some(ChatMessage {
                                role: "assistant".to_string(),
                                content: None,
                                tool_calls: Some(vec![tool_call]),
                                tool_call_id: None,
                                reasoning_content: reasoning.clone(),
                                reasoning,
                            });
                        }
                    }
                    pending_tool_outputs_in_turn += 1;
                }
                ResponseItem::FunctionCallOutput { call_id, output } => {
                    // Tool result: flush pending assistant, then push tool message.
                    // Images cannot go in tool-role messages (Chat Completions API only
                    // supports string content on tool messages). Split them into a
                    // separate user message instead.
                    if let Some(msg) = pending_assistant.take() {
                        messages.push(msg);
                    }
                    let (tool_content, user_image_content) =
                        split_tool_output_into_tool_and_user_content(&output.body);
                    messages.push(ChatMessage {
                        role: "tool".to_string(),
                        content: tool_content,
                        tool_calls: None,
                        tool_call_id: Some(call_id.clone()),
                        reasoning_content: None,
                        reasoning: None,
                    });
                    if let Some(image_content) = user_image_content {
                        messages.push(ChatMessage {
                            role: "user".to_string(),
                            content: Some(image_content),
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning_content: None,
                            reasoning: None,
                        });
                    }
                    pending_tool_outputs_in_turn = pending_tool_outputs_in_turn.saturating_sub(1);
                    if pending_tool_outputs_in_turn == 0 && !pending_warnings.is_empty() {
                        messages.append(&mut pending_warnings);
                    }
                }
                ResponseItem::CustomToolCallOutput {
                    call_id,
                    name: _,
                    output,
                } => {
                    // Tool result: flush pending assistant, then push tool message.
                    // Images cannot go in tool-role messages (Chat Completions API only
                    // supports string content on tool messages). Split them into a
                    // separate user message instead.
                    if let Some(msg) = pending_assistant.take() {
                        messages.push(msg);
                    }
                    let (tool_content, user_image_content) =
                        split_tool_output_into_tool_and_user_content(&output.body);
                    messages.push(ChatMessage {
                        role: "tool".to_string(),
                        content: tool_content,
                        tool_calls: None,
                        tool_call_id: Some(call_id.clone()),
                        reasoning_content: None,
                        reasoning: None,
                    });
                    if let Some(image_content) = user_image_content {
                        messages.push(ChatMessage {
                            role: "user".to_string(),
                            content: Some(image_content),
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning_content: None,
                            reasoning: None,
                        });
                    }
                    pending_tool_outputs_in_turn = pending_tool_outputs_in_turn.saturating_sub(1);
                    if pending_tool_outputs_in_turn == 0 && !pending_warnings.is_empty() {
                        messages.append(&mut pending_warnings);
                    }
                }
                ResponseItem::Reasoning { .. } => {
                    // Reasoning belongs to the current assistant turn, so it should not flush a
                    // pending assistant message or pending warning queue on its own.
                }
                _ => {
                    // Skip items that don't map cleanly to chat format.
                    // Flush pending assistant before skipping unknown items.
                    if let Some(msg) = pending_assistant.take() {
                        messages.push(msg);
                    }
                    pending_tool_outputs_in_turn = 0;
                    if !pending_warnings.is_empty() {
                        messages.append(&mut pending_warnings);
                    }
                }
            }
        }
        // Flush any remaining pending assistant message.
        if let Some(msg) = pending_assistant.take() {
            messages.push(msg);
        }
        if !pending_warnings.is_empty() {
            messages.append(&mut pending_warnings);
        }

        // DeepSeek thinking mode requires reasoning_content on ALL assistant messages
        // when the model is in thinking mode. Even if a non-thinking model was used
        // mid-session, switching back to DeepSeek requires reasoning_content to be
        // present on every assistant message in the conversation history.
        // Fill in "No reasoning required" for any assistant message missing this field.
        for msg in &mut messages {
            if msg.role == "assistant" && msg.reasoning_content.is_none() {
                msg.reasoning_content = Some("No reasoning required".to_string());
            }
        }

        // Convert tools and build namespace map for MCP tool resolution
        let tools = create_tools_json_for_chat_completions(&prompt.tools)?;
        let tool_namespace_map = build_tool_namespace_map(&prompt.tools);

        // Check if there are tool_calls but no tools defined
        let has_tool_calls = messages.iter().any(|m| m.tool_calls.is_some());
        if has_tool_calls && tools.is_empty() {
            return Err(CodexErr::InvalidRequest(
                "Request has tool_calls but no tools defined. This will cause a 400 error from the API."
                    .to_string(),
            ));
        }

        // Determine reasoning_effort for models that support reasoning
        let reasoning_effort = if model_info.supports_reasoning_summaries {
            effort.or(model_info.default_reasoning_level)
        } else {
            None
        };

        let request = ChatCompletionsRequest {
            model: model_info.slug.clone(),
            messages,
            tools,
            tool_choice: Some(serde_json::Value::String("auto".to_string())),
            stream: false,
            temperature: None,
            max_tokens: None,
            stop: None,
            reasoning_effort,
            parallel_tool_calls: Some(prompt.parallel_tool_calls),
            service_tier: None, // TODO: wire through from config or turn settings
            tool_namespace_map,
        };
        Ok(request)
    }
}

/// Converts a slice of [`ContentItem`] into a [`serde_json::Value`] suitable for the
/// `content` field of a Chat Completions API message.
///
/// - Text-only content is serialized as a plain `Value::String` (preserving existing
///   wire format).
/// - Content that includes at least one `InputImage` is serialized as a multipart
///   `Value::Array` following the OpenAI Chat Completions multipart format:
///   `[{"type":"text","text":"..."}, {"type":"image_url","image_url":{"url":"...","detail":"high"}}]`.
///
/// For assistant-role messages, `InputImage` items are intentionally dropped because
/// the Chat Completions API does not support `image_url` parts in assistant messages.
/// In that case, even if images were present, the result falls back to text-only string format.
///
/// Returns `None` when the content slice is empty or contains only dropped items.
fn content_items_to_chat_content(
    content: &[codex_protocol::models::ContentItem],
    is_assistant: bool,
) -> Option<serde_json::Value> {
    if content.is_empty() {
        return None;
    }

    let mut has_image = false;
    let mut text_parts: Vec<String> = Vec::new();
    let mut multipart_parts: Vec<serde_json::Value> = Vec::new();

    for item in content {
        match item {
            codex_protocol::models::ContentItem::InputText { text }
            | codex_protocol::models::ContentItem::OutputText { text } => {
                text_parts.push(text.clone());
                multipart_parts.push(serde_json::json!({"type": "text", "text": text.clone()}));
            }
            codex_protocol::models::ContentItem::InputImage { image_url, detail } => {
                if is_assistant {
                    // Assistant messages in the Chat Completions API only support text
                    // content parts; drop image items rather than erroring.
                    continue;
                }
                has_image = true;
                let mut image_url_obj = serde_json::json!({"url": image_url});
                if let Some(d) = detail {
                    let detail_str = match d {
                        codex_protocol::models::ImageDetail::Auto => "auto",
                        codex_protocol::models::ImageDetail::Low => "low",
                        codex_protocol::models::ImageDetail::High => "high",
                        codex_protocol::models::ImageDetail::Original => "original",
                    };
                    image_url_obj["detail"] = serde_json::Value::String(detail_str.to_string());
                }
                multipart_parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": image_url_obj,
                }));
            }
        }
    }

    if multipart_parts.is_empty() {
        return None;
    }

    if has_image && !is_assistant {
        Some(serde_json::Value::Array(multipart_parts))
    } else {
        let joined = text_parts.join("\n");
        if joined.is_empty() {
            None
        } else {
            Some(serde_json::Value::String(joined))
        }
    }
}

/// Splits a [`FunctionCallOutputBody`] into a text-only tool message content and,
/// if present, a multipart user message containing any images.
///
/// The Chat Completions API only supports plain-string `content` on `role: "tool"`
/// messages. Images from tool results (e.g. `view_image`) cannot be placed in the
/// tool message itself. Instead, they must be delivered via a separate `role: "user"`
/// message using the multipart content format:
///
/// ```json
/// [{"type":"text","text":"[tool result image]"},
///  {"type":"image_url","image_url":{"url":"data:image/png;base64,...","detail":"high"}}]
/// ```
///
/// Returns `(tool_content, optional_user_content)`:
/// - `tool_content`: `Some(Value::String(..))` with the text portion for the tool message,
///   or `None` when the body is empty.
/// - `optional_user_content`: `Some(Value::Array(..))` with multipart content including
///   images when the body contains `InputImage` items, otherwise `None`.
fn split_tool_output_into_tool_and_user_content(
    body: &codex_protocol::models::FunctionCallOutputBody,
) -> (Option<serde_json::Value>, Option<serde_json::Value>) {
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use codex_protocol::models::ImageDetail;

    match body {
        FunctionCallOutputBody::Text(text) => {
            if text.is_empty() {
                (None, None)
            } else {
                (Some(serde_json::Value::String(text.clone())), None)
            }
        }
        FunctionCallOutputBody::ContentItems(items) => {
            if items.is_empty() {
                return (None, None);
            }

            let mut text_parts: Vec<String> = Vec::new();
            let mut image_parts: Vec<serde_json::Value> = Vec::new();

            for item in items {
                match item {
                    FunctionCallOutputContentItem::InputText { text } => {
                        text_parts.push(text.clone());
                    }
                    FunctionCallOutputContentItem::InputImage { image_url, detail } => {
                        let mut image_url_obj = serde_json::json!({"url": image_url});
                        if let Some(d) = detail {
                            let detail_str = match d {
                                ImageDetail::Auto => "auto",
                                ImageDetail::Low => "low",
                                ImageDetail::High => "high",
                                ImageDetail::Original => "original",
                            };
                            image_url_obj["detail"] =
                                serde_json::Value::String(detail_str.to_string());
                        }
                        image_parts.push(serde_json::json!({
                            "type": "image_url",
                            "image_url": image_url_obj,
                        }));
                    }
                }
            }

            let tool_content = if text_parts.is_empty() {
                // If there are only images, the tool message needs at least an empty
                // string so it is not null (some providers reject null tool content).
                Some(serde_json::Value::String("[image result]".to_string()))
            } else {
                Some(serde_json::Value::String(text_parts.join("\n")))
            };

            let user_content = if image_parts.is_empty() {
                None
            } else {
                // Build multipart user message: a text label + all image parts
                let mut parts: Vec<serde_json::Value> = Vec::new();
                for img_part in image_parts {
                    parts.push(img_part);
                }
                Some(serde_json::Value::Array(parts))
            };

            (tool_content, user_content)
        }
    }
}
/// Builds a map from flat tool name to namespace prefix for MCP tools.
/// When the Chat Completions API returns a tool call with a flat name like
/// `ast_grep_search`, this map lets us look up the namespace (e.g. `omx_code_intel__`)
/// so the resulting `ResponseItem::FunctionCall` carries the correct namespace
/// for tool resolution.
fn build_tool_namespace_map(tools: &[ToolSpec]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for tool in tools {
        if let ToolSpec::Namespace(ns) = tool {
            for ns_tool in &ns.tools {
                let ResponsesApiNamespaceTool::Function(func) = ns_tool;
                map.insert(func.name.clone(), ns.name.clone());
            }
        }
    }
    map
}

#[cfg(test)]
mod chat_completions_request_tests {
    use super::ModelClient;
    use super::ModelClientSession;
    use crate::client_common::Prompt;
    use crate::tools::handlers::shell_spec::CommandToolOptions;
    use crate::tools::handlers::shell_spec::create_exec_command_tool;
    use codex_api::ChatCompletionsRequest;
    use codex_model_provider_info::WireApi;
    use codex_model_provider_info::create_oss_provider_with_base_url;
    use codex_protocol::SessionId;
    use codex_protocol::ThreadId;
    use codex_protocol::models::BaseInstructions;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::ImageDetail;
    use codex_protocol::models::ReasoningItemContent;
    use codex_protocol::models::ReasoningItemReasoningSummary;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::openai_models::ModelInfo;
    use codex_protocol::protocol::SessionSource;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn test_model_client() -> ModelClient {
        let provider =
            create_oss_provider_with_base_url("https://example.com/v1", WireApi::Responses);
        ModelClient::new(
            /*auth_manager*/ None,
            SessionId::new(),
            ThreadId::new(),
            /*installation_id*/ "11111111-1111-4111-8111-111111111111".to_string(),
            provider,
            SessionSource::Cli,
            /*model_verbosity*/ None,
            /*enable_request_compression*/ false,
            /*include_timing_metrics*/ false,
            /*beta_features_header*/ None,
        )
    }

    fn test_model_info() -> ModelInfo {
        serde_json::from_value(json!({
            "slug": "gpt-test",
            "display_name": "gpt-test",
            "description": "desc",
            "default_reasoning_level": "medium",
            "supported_reasoning_levels": [
                {"effort": "medium", "description": "medium"}
            ],
            "shell_type": "shell_command",
            "visibility": "list",
            "supported_in_api": true,
            "priority": 1,
            "upgrade": null,
            "base_instructions": "base instructions",
            "model_messages": null,
            "supports_reasoning_summaries": false,
            "support_verbosity": false,
            "default_verbosity": null,
            "apply_patch_tool_type": null,
            "truncation_policy": {"mode": "bytes", "limit": 10000},
            "supports_parallel_tool_calls": false,
            "supports_image_detail_original": false,
            "context_window": 272000,
            "auto_compact_token_limit": null,
            "experimental_supported_tools": []
        }))
        .expect("deserialize test model info")
    }

    fn test_prompt(input: Vec<ResponseItem>) -> Prompt {
        Prompt {
            input,
            tools: vec![create_exec_command_tool(CommandToolOptions {
                allow_login_shell: false,
                exec_permission_approvals_enabled: false,
            })],
            parallel_tool_calls: false,
            base_instructions: BaseInstructions {
                text: String::new(),
            },
            personality: None,
            output_schema: None,
            output_schema_strict: true,
        }
    }

    fn build_request(input: Vec<ResponseItem>) -> ChatCompletionsRequest {
        test_model_session()
            .build_chat_completions_request(&test_prompt(input), &test_model_info(), None)
            .expect("build chat completions request")
    }

    fn test_model_session() -> ModelClientSession {
        test_model_client().new_session()
    }

    fn warning_item(message: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: message.to_string(),
            }],
            phase: None,
        }
    }

    fn assistant_message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: text.to_string(),
            }],
            phase: None,
        }
    }

    fn reasoning_item(text: &str) -> ResponseItem {
        ResponseItem::Reasoning {
            id: "reasoning-id".to_string(),
            summary: vec![ReasoningItemReasoningSummary::SummaryText {
                text: text.to_string(),
            }],
            content: Some(vec![ReasoningItemContent::ReasoningText {
                text: text.to_string(),
            }]),
            encrypted_content: None,
        }
    }

    fn function_call(call_id: &str, arguments: &str) -> ResponseItem {
        ResponseItem::FunctionCall {
            id: None,
            name: "exec_command".to_string(),
            namespace: None,
            arguments: arguments.to_string(),
            call_id: call_id.to_string(),
        }
    }

    fn function_call_output(call_id: &str, output: &str) -> ResponseItem {
        ResponseItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload::from_text(output.to_string()),
        }
    }

    fn custom_tool_call(call_id: &str, input: &str) -> ResponseItem {
        ResponseItem::CustomToolCall {
            id: None,
            status: None,
            call_id: call_id.to_string(),
            name: "exec_command".to_string(),
            input: input.to_string(),
        }
    }

    fn custom_tool_call_output(call_id: &str, output: &str) -> ResponseItem {
        ResponseItem::CustomToolCallOutput {
            call_id: call_id.to_string(),
            name: None,
            output: FunctionCallOutputPayload::from_text(output.to_string()),
        }
    }

    fn function_call_output_with_image(call_id: &str, text: &str, image_url: &str) -> ResponseItem {
        use codex_protocol::models::FunctionCallOutputContentItem;
        ResponseItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload::from_content_items(vec![
                FunctionCallOutputContentItem::InputText {
                    text: text.to_string(),
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: image_url.to_string(),
                    detail: Some(ImageDetail::High),
                },
            ]),
        }
    }

    #[test]
    fn chat_completions_request_moves_tool_warning_after_matching_tool_result() {
        let request = build_request(vec![
            function_call("call-1", r#"{"cmd":"pwd"}"#),
            warning_item(
                "Warning: apply_patch was requested via exec_command. Use the apply_patch tool instead of exec_command.",
            ),
            function_call_output("call-1", "ok"),
        ]);

        assert_eq!(
            serde_json::to_value(&request.messages).expect("serialize messages"),
            json!([
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "exec_command",
                                "arguments": "{\"cmd\":\"pwd\"}"
                            }
                        }
                    ],
                    "reasoning_content": "No reasoning required"
                },
                {
                    "role": "tool",
                    "content": "ok",
                    "tool_call_id": "call-1"
                },
                {
                    "role": "user",
                    "content": "Warning: apply_patch was requested via exec_command. Use the apply_patch tool instead of exec_command."
                }
            ])
        );
    }

    #[test]
    fn chat_completions_request_keeps_warning_with_later_tool_result() {
        let request = build_request(vec![
            function_call("call-1", r#"{"cmd":"pwd"}"#),
            function_call_output("call-1", "first"),
            function_call("call-2", r#"{"cmd":"ls"}"#),
            warning_item("Warning: tool pressure warning"),
            function_call_output("call-2", "second"),
        ]);

        assert_eq!(
            serde_json::to_value(&request.messages).expect("serialize messages"),
            json!([
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "exec_command",
                                "arguments": "{\"cmd\":\"pwd\"}"
                            }
                        }
                    ],
                    "reasoning_content": "No reasoning required"
                },
                {
                    "role": "tool",
                    "content": "first",
                    "tool_call_id": "call-1"
                },
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call-2",
                            "type": "function",
                            "function": {
                                "name": "exec_command",
                                "arguments": "{\"cmd\":\"ls\"}"
                            }
                        }
                    ],
                    "reasoning_content": "No reasoning required"
                },
                {
                    "role": "tool",
                    "content": "second",
                    "tool_call_id": "call-2"
                },
                {
                    "role": "user",
                    "content": "Warning: tool pressure warning"
                }
            ])
        );
    }

    #[test]
    fn chat_completions_request_flushes_warnings_after_last_tool_result_in_turn() {
        let request = build_request(vec![
            function_call("call-1", r#"{"cmd":"pwd"}"#),
            function_call("call-2", r#"{"cmd":"ls"}"#),
            warning_item("Warning: first tool warning"),
            function_call_output("call-1", "first"),
            warning_item("Warning: second tool warning"),
            function_call_output("call-2", "second"),
        ]);

        assert_eq!(
            serde_json::to_value(&request.messages).expect("serialize messages"),
            json!([
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "exec_command",
                                "arguments": "{\"cmd\":\"pwd\"}"
                            }
                        },
                        {
                            "id": "call-2",
                            "type": "function",
                            "function": {
                                "name": "exec_command",
                                "arguments": "{\"cmd\":\"ls\"}"
                            }
                        }
                    ],
                    "reasoning_content": "No reasoning required"
                },
                {
                    "role": "tool",
                    "content": "first",
                    "tool_call_id": "call-1"
                },
                {
                    "role": "tool",
                    "content": "second",
                    "tool_call_id": "call-2"
                },
                {
                    "role": "user",
                    "content": "Warning: first tool warning"
                },
                {
                    "role": "user",
                    "content": "Warning: second tool warning"
                }
            ])
        );
    }

    #[test]
    fn chat_completions_request_flushes_custom_tool_warnings_after_last_tool_result_in_turn() {
        let request = build_request(vec![
            custom_tool_call("call-1", "pwd"),
            custom_tool_call("call-2", "ls"),
            warning_item("Warning: custom tool warning"),
            custom_tool_call_output("call-1", "first"),
            custom_tool_call_output("call-2", "second"),
        ]);

        assert_eq!(
            serde_json::to_value(&request.messages).expect("serialize messages"),
            json!([
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "exec_command",
                                "arguments": "pwd"
                            }
                        },
                        {
                            "id": "call-2",
                            "type": "function",
                            "function": {
                                "name": "exec_command",
                                "arguments": "ls"
                            }
                        }
                    ],
                    "reasoning_content": "No reasoning required"
                },
                {
                    "role": "tool",
                    "content": "first",
                    "tool_call_id": "call-1"
                },
                {
                    "role": "tool",
                    "content": "second",
                    "tool_call_id": "call-2"
                },
                {
                    "role": "user",
                    "content": "Warning: custom tool warning"
                }
            ])
        );
    }

    #[test]
    fn chat_completions_request_flushes_mixed_tool_warnings_after_last_tool_result_in_turn() {
        let request = build_request(vec![
            function_call("call-1", r#"{"cmd":"pwd"}"#),
            custom_tool_call("call-2", "ls"),
            warning_item("Warning: mixed tool warning"),
            function_call_output("call-1", "first"),
            custom_tool_call_output("call-2", "second"),
        ]);

        assert_eq!(
            serde_json::to_value(&request.messages).expect("serialize messages"),
            json!([
                {
                    "role": "assistant",
                    "tool_calls": [
                        {
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "exec_command",
                                "arguments": "{\"cmd\":\"pwd\"}"
                            }
                        },
                        {
                            "id": "call-2",
                            "type": "function",
                            "function": {
                                "name": "exec_command",
                                "arguments": "ls"
                            }
                        }
                    ],
                    "reasoning_content": "No reasoning required"
                },
                {
                    "role": "tool",
                    "content": "first",
                    "tool_call_id": "call-1"
                },
                {
                    "role": "tool",
                    "content": "second",
                    "tool_call_id": "call-2"
                },
                {
                    "role": "user",
                    "content": "Warning: mixed tool warning"
                }
            ])
        );
    }

    #[test]
    fn chat_completions_request_keeps_assistant_text_before_tool_warning() {
        let request = build_request(vec![
            assistant_message("Running command"),
            function_call("call-1", r#"{"cmd":"pwd"}"#),
            warning_item("Warning: merged assistant warning"),
            function_call_output("call-1", "ok"),
        ]);

        assert_eq!(
            serde_json::to_value(&request.messages).expect("serialize messages"),
            json!([
                {
                    "role": "assistant",
                    "content": "Running command",
                    "tool_calls": [
                        {
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "exec_command",
                                "arguments": "{\"cmd\":\"pwd\"}"
                            }
                        }
                    ],
                    "reasoning_content": "No reasoning required"
                },
                {
                    "role": "tool",
                    "content": "ok",
                    "tool_call_id": "call-1"
                },
                {
                    "role": "user",
                    "content": "Warning: merged assistant warning"
                }
            ])
        );
    }

    #[test]
    fn chat_completions_request_keeps_assistant_text_after_tool_warning() {
        let request = build_request(vec![
            function_call("call-1", r#"{"cmd":"pwd"}"#),
            assistant_message("Running command"),
            warning_item("Warning: merged assistant warning"),
            function_call_output("call-1", "ok"),
        ]);

        assert_eq!(
            serde_json::to_value(&request.messages).expect("serialize messages"),
            json!([
                {
                    "role": "assistant",
                    "content": "Running command",
                    "tool_calls": [
                        {
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "exec_command",
                                "arguments": "{\"cmd\":\"pwd\"}"
                            }
                        }
                    ],
                    "reasoning_content": "No reasoning required"
                },
                {
                    "role": "tool",
                    "content": "ok",
                    "tool_call_id": "call-1"
                },
                {
                    "role": "user",
                    "content": "Warning: merged assistant warning"
                }
            ])
        );
    }

    #[test]
    fn chat_completions_request_preserves_warning_before_new_assistant_turn() {
        let request = build_request(vec![
            warning_item("Warning: fallback model was used"),
            assistant_message("Fallback complete"),
        ]);

        assert_eq!(
            serde_json::to_value(&request.messages).expect("serialize messages"),
            json!([
                {
                    "role": "user",
                    "content": "Warning: fallback model was used"
                },
                {
                    "role": "assistant",
                    "content": "Fallback complete",
                    "reasoning_content": "No reasoning required"
                }
            ])
        );
    }

    #[test]
    fn chat_completions_request_keeps_reasoning_between_assistant_text_and_tool_call() {
        let request = build_request(vec![
            assistant_message("Running command"),
            reasoning_item("Need to call a tool"),
            function_call("call-1", r#"{"cmd":"pwd"}"#),
            function_call_output("call-1", "ok"),
        ]);

        assert_eq!(
            serde_json::to_value(&request.messages).expect("serialize messages"),
            json!([
                {
                    "role": "assistant",
                    "content": "Running command",
                    "tool_calls": [
                        {
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "exec_command",
                                "arguments": "{\"cmd\":\"pwd\"}"
                            }
                        }
                    ],
                    "reasoning": "Need to call a tool",
                    "reasoning_content": "Need to call a tool"
                },
                {
                    "role": "tool",
                    "content": "ok",
                    "tool_call_id": "call-1"
                }
            ])
        );
    }
    #[test]
    fn chat_completions_request_user_message_with_image_uses_multipart_content() {
        let request = build_request(vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "Describe this image".to_string(),
                },
                ContentItem::InputImage {
                    image_url: "data:image/png;base64,abc".to_string(),
                    detail: Some(ImageDetail::High),
                },
            ],
            phase: None,
        }]);

        let messages = &request.messages;
        // System message + user message
        assert_eq!(messages.len(), 1);
        let user_msg = &messages[0];
        assert_eq!(user_msg.role, "user");
        let content = user_msg
            .content
            .as_ref()
            .expect("user message should have content");
        // Should be multipart array format
        assert!(
            content.is_array(),
            "content with image should be multipart array"
        );
        let arr = content.as_array().expect("content should be array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "Describe this image");
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(arr[1]["image_url"]["url"], "data:image/png;base64,abc");
        assert_eq!(arr[1]["image_url"]["detail"], "high");
    }

    #[test]
    fn chat_completions_request_user_message_text_only_remains_string() {
        let request = build_request(vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "hello".to_string(),
            }],
            phase: None,
        }]);

        let messages = &request.messages;
        assert_eq!(messages.len(), 1);
        let user_msg = &messages[0];
        assert_eq!(user_msg.role, "user");
        let content = user_msg.content.as_ref().expect("should have content");
        // Text-only should remain as plain string
        assert!(content.is_string(), "text-only content should be a string");
        assert_eq!(content.as_str().unwrap(), "hello");
    }

    #[test]
    fn chat_completions_request_assistant_message_drops_image() {
        let request = build_request(vec![ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![
                ContentItem::OutputText {
                    text: "Here is the result".to_string(),
                },
                ContentItem::InputImage {
                    image_url: "data:image/png;base64,abc".to_string(),
                    detail: Some(ImageDetail::High),
                },
            ],
            phase: None,
        }]);

        let messages = &request.messages;
        let assistant_msg = &messages[0];
        assert_eq!(assistant_msg.role, "assistant");
        let content = assistant_msg.content.as_ref().expect("should have content");
        // Assistant messages should be text-only (images dropped)
        assert!(
            content.is_string(),
            "assistant content should be plain string"
        );
        assert_eq!(content.as_str().unwrap(), "Here is the result");
    }

    #[test]
    fn chat_completions_request_user_message_image_only() {
        let request = build_request(vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "data:image/png;base64,xyz".to_string(),
                detail: Some(ImageDetail::Auto),
            }],
            phase: None,
        }]);

        let messages = &request.messages;
        let user_msg = &messages[0];
        assert_eq!(user_msg.role, "user");
        let content = user_msg.content.as_ref().expect("should have content");
        assert!(
            content.is_array(),
            "image-only content should be multipart array"
        );
        let arr = content.as_array().expect("content should be array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "image_url");
        assert_eq!(arr[0]["image_url"]["url"], "data:image/png;base64,xyz");
        assert_eq!(arr[0]["image_url"]["detail"], "auto");
    }

    #[test]
    fn chat_completions_request_user_message_image_detail_none_omits_field() {
        let request = build_request(vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputImage {
                image_url: "data:image/png;base64,test".to_string(),
                detail: None,
            }],
            phase: None,
        }]);

        let messages = &request.messages;
        let user_msg = &messages[0];
        let content = user_msg.content.as_ref().expect("should have content");
        let arr = content.as_array().expect("content should be array");
        // When detail is None, the detail field should be omitted
        assert_eq!(arr[0]["image_url"]["url"], "data:image/png;base64,test");
        assert!(
            arr[0]["image_url"].get("detail").is_none(),
            "detail should be omitted when None"
        );
    }

    #[test]
    fn chat_completions_request_user_message_multiple_images() {
        let request = build_request(vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "Compare these".to_string(),
                },
                ContentItem::InputImage {
                    image_url: "data:image/png;base64,aaa".to_string(),
                    detail: Some(ImageDetail::High),
                },
                ContentItem::InputImage {
                    image_url: "data:image/jpeg;base64,bbb".to_string(),
                    detail: Some(ImageDetail::Low),
                },
            ],
            phase: None,
        }]);

        let messages = &request.messages;
        let user_msg = &messages[0];
        let content = user_msg.content.as_ref().expect("should have content");
        let arr = content
            .as_array()
            .expect("content should be multipart array");
        assert_eq!(arr.len(), 3);
        // Text part
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[0]["text"], "Compare these");
        // First image
        assert_eq!(arr[1]["type"], "image_url");
        assert_eq!(arr[1]["image_url"]["url"], "data:image/png;base64,aaa");
        assert_eq!(arr[1]["image_url"]["detail"], "high");
        // Second image
        assert_eq!(arr[2]["type"], "image_url");
        assert_eq!(arr[2]["image_url"]["url"], "data:image/jpeg;base64,bbb");
        assert_eq!(arr[2]["image_url"]["detail"], "low");
    }

    #[test]
    fn chat_completions_request_tool_output_with_image_splits_into_tool_and_user() {
        let request = build_request(vec![
            function_call("call-1", r#"{"cmd":"view_image"}"#),
            function_call_output_with_image(
                "call-1",
                "Here is the image:",
                "data:image/png;base64,abc123",
            ),
        ]);

        let messages = &request.messages;
        // First message: assistant with tool_calls
        assert_eq!(messages[0].role, "assistant");
        assert!(messages[0].tool_calls.is_some());
        // Second message: tool with text-only content (images cannot go in tool messages)
        assert_eq!(messages[1].role, "tool");
        let tool_content = messages[1]
            .content
            .as_ref()
            .expect("tool should have content");
        assert!(
            tool_content.is_string(),
            "tool content should be plain text"
        );
        assert_eq!(tool_content.as_str(), Some("Here is the image:"));
        // Third message: user with the image in multipart format
        assert_eq!(messages[2].role, "user");
        let user_content = messages[2]
            .content
            .as_ref()
            .expect("user message should have image content");
        assert!(
            user_content.is_array(),
            "user image content should be multipart array"
        );
        let arr = user_content.as_array().expect("content should be array");
        assert_eq!(arr.len(), 1);
        // Image part
        assert_eq!(arr[0]["type"], "image_url");
        assert_eq!(arr[0]["image_url"]["url"], "data:image/png;base64,abc123");
        assert_eq!(arr[0]["image_url"]["detail"], "high");
    }
}

/// Parses per-turn metadata into an HTTP header value.
///
/// Invalid values are treated as absent so callers can compare and propagate
/// metadata with the same sanitization path used when constructing headers.
fn parse_turn_metadata_header(turn_metadata_header: Option<&str>) -> Option<HeaderValue> {
    turn_metadata_header.and_then(|value| HeaderValue::from_str(value).ok())
}

/// Stamp a ResponsesWsRequest with the current time.
///
/// Meant to be called just before sending the request over the socket, to capture realistic
/// transport timing.
fn stamp_ws_stream_request_start_ms(request: &mut ResponsesWsRequest) {
    let ResponsesWsRequest::ResponseCreate(payload) = request else {
        return;
    };
    payload
        .client_metadata
        .get_or_insert_with(HashMap::new)
        .insert(
            X_CODEX_WS_STREAM_REQUEST_START_MS_CLIENT_METADATA_KEY.to_string(),
            crate::turn_timing::now_unix_timestamp_ms().to_string(),
        );
}

/// Builds the extra headers attached to Responses API requests.
///
/// These headers implement Codex-specific conventions:
///
/// - `x-codex-beta-features`: comma-separated beta feature keys enabled for the session.
/// - `x-codex-turn-state`: sticky routing token captured earlier in the turn.
/// - `x-codex-turn-metadata`: optional per-turn metadata for observability.
fn build_responses_headers(
    beta_features_header: Option<&str>,
    turn_state: Option<&Arc<OnceLock<String>>>,
    turn_metadata_header: Option<&HeaderValue>,
) -> ApiHeaderMap {
    let mut headers = ApiHeaderMap::new();
    if let Some(value) = beta_features_header
        && !value.is_empty()
        && let Ok(header_value) = HeaderValue::from_str(value)
    {
        headers.insert("x-codex-beta-features", header_value);
    }
    if let Some(turn_state) = turn_state
        && let Some(state) = turn_state.get()
        && let Ok(header_value) = HeaderValue::from_str(state)
    {
        headers.insert(X_CODEX_TURN_STATE_HEADER, header_value);
    }
    if let Some(header_value) = turn_metadata_header {
        headers.insert(X_CODEX_TURN_METADATA_HEADER, header_value.clone());
    }
    headers
}

fn subagent_header_value(session_source: &SessionSource) -> Option<String> {
    match session_source {
        SessionSource::SubAgent(subagent_source) => match subagent_source {
            SubAgentSource::Review => Some("review".to_string()),
            SubAgentSource::Compact => Some("compact".to_string()),
            SubAgentSource::MemoryConsolidation => Some("memory_consolidation".to_string()),
            SubAgentSource::ThreadSpawn { .. } => Some("collab_spawn".to_string()),
            SubAgentSource::Other(label) => Some(label.clone()),
        },
        SessionSource::Internal(InternalSessionSource::MemoryConsolidation) => {
            Some("memory_consolidation".to_string())
        }
        SessionSource::Cli
        | SessionSource::VSCode
        | SessionSource::Exec
        | SessionSource::Mcp
        | SessionSource::Custom(_)
        | SessionSource::Unknown => None,
    }
}

fn parent_thread_id_header_value(session_source: &SessionSource) -> Option<String> {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) => Some(parent_thread_id.to_string()),
        SessionSource::Cli
        | SessionSource::VSCode
        | SessionSource::Exec
        | SessionSource::Mcp
        | SessionSource::Custom(_)
        | SessionSource::Internal(_)
        | SessionSource::SubAgent(_)
        | SessionSource::Unknown => None,
    }
}

const RESPONSE_STREAM_CHANNEL_CAPACITY: usize = 1600;
const STREAM_DROPPED_REASON: &str = "response stream dropped before provider terminal event";

fn map_response_stream(
    api_stream: codex_api::ResponseStream,
    session_telemetry: SessionTelemetry,
    inference_trace_attempt: InferenceTraceAttempt,
) -> (ResponseStream, oneshot::Receiver<LastResponse>) {
    let codex_api::ResponseStream {
        rx_event,
        upstream_request_id,
    } = api_stream;
    let api_stream = codex_api::ResponseStream {
        rx_event,
        upstream_request_id: None,
    };
    map_response_events(
        upstream_request_id,
        api_stream,
        session_telemetry,
        inference_trace_attempt,
    )
}

fn map_response_events<S>(
    upstream_request_id: Option<String>,
    api_stream: S,
    session_telemetry: SessionTelemetry,
    inference_trace_attempt: InferenceTraceAttempt,
) -> (ResponseStream, oneshot::Receiver<LastResponse>)
where
    S: futures::Stream<Item = std::result::Result<ResponseEvent, ApiError>>
        + Unpin
        + Send
        + 'static,
{
    let (tx_event, rx_event) =
        mpsc::channel::<Result<ResponseEvent>>(RESPONSE_STREAM_CHANNEL_CAPACITY);
    let (tx_last_response, rx_last_response) = oneshot::channel::<LastResponse>();
    let consumer_dropped = CancellationToken::new();
    let consumer_dropped_for_stream = consumer_dropped.clone();

    tokio::spawn(async move {
        let mut logged_error = false;
        let mut tx_last_response = Some(tx_last_response);
        let mut items_added: Vec<ResponseItem> = Vec::new();
        let mut api_stream = api_stream;
        let upstream_request_id = upstream_request_id.as_deref();
        if let Some(upstream_request_id) = upstream_request_id {
            feedback_tags!(last_model_request_id = upstream_request_id);
        }
        loop {
            let event = tokio::select! {
                _ = consumer_dropped.cancelled() => {
                    inference_trace_attempt.record_cancelled(
                        STREAM_DROPPED_REASON,
                        upstream_request_id,
                        &items_added,
                    );
                    return;
                }
                event = api_stream.next() => event,
            };
            let Some(event) = event else {
                break;
            };
            match event {
                Ok(ResponseEvent::OutputItemDone(item)) => {
                    items_added.push(item.clone());
                    if tx_event
                        .send(Ok(ResponseEvent::OutputItemDone(item)))
                        .await
                        .is_err()
                    {
                        inference_trace_attempt.record_cancelled(
                            STREAM_DROPPED_REASON,
                            upstream_request_id,
                            &items_added,
                        );
                        return;
                    }
                }
                Ok(ResponseEvent::Completed {
                    response_id,
                    token_usage,
                    end_turn,
                }) => {
                    feedback_tags!(last_model_response_id = &response_id);
                    if let Some(usage) = &token_usage {
                        session_telemetry.sse_event_completed(
                            usage.input_tokens,
                            usage.output_tokens,
                            Some(usage.cached_input_tokens),
                            Some(usage.reasoning_output_tokens),
                            usage.total_tokens,
                        );
                    }
                    inference_trace_attempt.record_completed(
                        &response_id,
                        upstream_request_id,
                        &token_usage,
                        &items_added,
                    );
                    if let Some(sender) = tx_last_response.take() {
                        let _ = sender.send(LastResponse {
                            response_id: response_id.clone(),
                            items_added: std::mem::take(&mut items_added),
                        });
                    }
                    if tx_event
                        .send(Ok(ResponseEvent::Completed {
                            response_id,
                            token_usage,
                            end_turn,
                        }))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(event) => {
                    if tx_event.send(Ok(event)).await.is_err() {
                        inference_trace_attempt.record_cancelled(
                            STREAM_DROPPED_REASON,
                            upstream_request_id,
                            &items_added,
                        );
                        return;
                    }
                }
                Err(err) => {
                    let response_debug_context =
                        extract_response_debug_context_from_api_error(&err);
                    let upstream_request_id =
                        upstream_request_id.or(response_debug_context.request_id.as_deref());
                    if let Some(upstream_request_id) = upstream_request_id {
                        feedback_tags!(last_model_request_id = upstream_request_id);
                    }
                    let mapped = map_api_error(err);
                    inference_trace_attempt.record_failed(
                        &mapped,
                        upstream_request_id,
                        &items_added,
                    );
                    if !logged_error {
                        session_telemetry.see_event_completed_failed(&mapped);
                        logged_error = true;
                    }
                    if tx_event.send(Err(mapped)).await.is_err() {
                        return;
                    }
                }
            }
        }
        inference_trace_attempt.record_failed(
            "stream closed before response.completed",
            upstream_request_id,
            &items_added,
        );
    });

    (
        ResponseStream {
            rx_event,
            consumer_dropped: consumer_dropped_for_stream,
        },
        rx_last_response,
    )
}

/// Handles a 401 response by optionally refreshing ChatGPT tokens once.
///
/// When refresh succeeds, the caller should retry the API call; otherwise
/// the mapped `CodexErr` is returned to the caller.
#[derive(Clone, Copy, Debug)]
struct UnauthorizedRecoveryExecution {
    mode: &'static str,
    phase: &'static str,
}

#[derive(Clone, Copy, Debug, Default)]
struct PendingUnauthorizedRetry {
    retry_after_unauthorized: bool,
    recovery_mode: Option<&'static str>,
    recovery_phase: Option<&'static str>,
}

impl PendingUnauthorizedRetry {
    fn from_recovery(recovery: UnauthorizedRecoveryExecution) -> Self {
        Self {
            retry_after_unauthorized: true,
            recovery_mode: Some(recovery.mode),
            recovery_phase: Some(recovery.phase),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct AuthRequestTelemetryContext {
    auth_mode: Option<&'static str>,
    auth_header_attached: bool,
    auth_header_name: Option<&'static str>,
    retry_after_unauthorized: bool,
    recovery_mode: Option<&'static str>,
    recovery_phase: Option<&'static str>,
}

impl AuthRequestTelemetryContext {
    fn new(
        auth_mode: Option<AuthMode>,
        api_auth: &dyn AuthProvider,
        retry: PendingUnauthorizedRetry,
    ) -> Self {
        let auth_telemetry = auth_header_telemetry(api_auth);
        Self {
            auth_mode: auth_mode.map(|mode| match mode {
                AuthMode::ApiKey => "ApiKey",
                AuthMode::Chatgpt | AuthMode::ChatgptAuthTokens | AuthMode::AgentIdentity => {
                    "Chatgpt"
                }
            }),
            auth_header_attached: auth_telemetry.attached,
            auth_header_name: auth_telemetry.name,
            retry_after_unauthorized: retry.retry_after_unauthorized,
            recovery_mode: retry.recovery_mode,
            recovery_phase: retry.recovery_phase,
        }
    }
}

struct WebsocketConnectParams<'a> {
    session_telemetry: &'a SessionTelemetry,
    api_provider: codex_api::Provider,
    api_auth: SharedAuthProvider,
    turn_metadata_header: Option<&'a str>,
    options: &'a ApiResponsesOptions,
    auth_context: AuthRequestTelemetryContext,
    request_route_telemetry: RequestRouteTelemetry,
}

async fn handle_unauthorized(
    transport: TransportError,
    auth_recovery: &mut Option<UnauthorizedRecovery>,
    session_telemetry: &SessionTelemetry,
) -> Result<UnauthorizedRecoveryExecution> {
    let debug = extract_response_debug_context(&transport);
    if let Some(recovery) = auth_recovery
        && recovery.has_next()
    {
        let mode = recovery.mode_name();
        let phase = recovery.step_name();
        return match recovery.next().await {
            Ok(step_result) => {
                session_telemetry.record_auth_recovery(
                    mode,
                    phase,
                    "recovery_succeeded",
                    debug.request_id.as_deref(),
                    debug.cf_ray.as_deref(),
                    debug.auth_error.as_deref(),
                    debug.auth_error_code.as_deref(),
                    /*recovery_reason*/ None,
                    step_result.auth_state_changed(),
                );
                emit_feedback_auth_recovery_tags(
                    mode,
                    phase,
                    "recovery_succeeded",
                    debug.request_id.as_deref(),
                    debug.cf_ray.as_deref(),
                    debug.auth_error.as_deref(),
                    debug.auth_error_code.as_deref(),
                );
                Ok(UnauthorizedRecoveryExecution { mode, phase })
            }
            Err(RefreshTokenError::Permanent(failed)) => {
                session_telemetry.record_auth_recovery(
                    mode,
                    phase,
                    "recovery_failed_permanent",
                    debug.request_id.as_deref(),
                    debug.cf_ray.as_deref(),
                    debug.auth_error.as_deref(),
                    debug.auth_error_code.as_deref(),
                    /*recovery_reason*/ None,
                    /*auth_state_changed*/ None,
                );
                emit_feedback_auth_recovery_tags(
                    mode,
                    phase,
                    "recovery_failed_permanent",
                    debug.request_id.as_deref(),
                    debug.cf_ray.as_deref(),
                    debug.auth_error.as_deref(),
                    debug.auth_error_code.as_deref(),
                );
                Err(CodexErr::RefreshTokenFailed(failed))
            }
            Err(RefreshTokenError::Transient(other)) => {
                session_telemetry.record_auth_recovery(
                    mode,
                    phase,
                    "recovery_failed_transient",
                    debug.request_id.as_deref(),
                    debug.cf_ray.as_deref(),
                    debug.auth_error.as_deref(),
                    debug.auth_error_code.as_deref(),
                    /*recovery_reason*/ None,
                    /*auth_state_changed*/ None,
                );
                emit_feedback_auth_recovery_tags(
                    mode,
                    phase,
                    "recovery_failed_transient",
                    debug.request_id.as_deref(),
                    debug.cf_ray.as_deref(),
                    debug.auth_error.as_deref(),
                    debug.auth_error_code.as_deref(),
                );
                Err(CodexErr::Io(other))
            }
        };
    }

    let (mode, phase, recovery_reason) = match auth_recovery.as_ref() {
        Some(recovery) => (
            recovery.mode_name(),
            recovery.step_name(),
            Some(recovery.unavailable_reason()),
        ),
        None => ("none", "none", Some("auth_manager_missing")),
    };
    session_telemetry.record_auth_recovery(
        mode,
        phase,
        "recovery_not_run",
        debug.request_id.as_deref(),
        debug.cf_ray.as_deref(),
        debug.auth_error.as_deref(),
        debug.auth_error_code.as_deref(),
        recovery_reason,
        /*auth_state_changed*/ None,
    );
    emit_feedback_auth_recovery_tags(
        mode,
        phase,
        "recovery_not_run",
        debug.request_id.as_deref(),
        debug.cf_ray.as_deref(),
        debug.auth_error.as_deref(),
        debug.auth_error_code.as_deref(),
    );

    Err(map_api_error(ApiError::Transport(transport)))
}

fn api_error_http_status(error: &ApiError) -> Option<u16> {
    match error {
        ApiError::Transport(TransportError::Http { status, .. }) => Some(status.as_u16()),
        _ => None,
    }
}

struct ApiTelemetry {
    session_telemetry: SessionTelemetry,
    auth_context: AuthRequestTelemetryContext,
    request_route_telemetry: RequestRouteTelemetry,
    auth_env_telemetry: AuthEnvTelemetry,
}

impl ApiTelemetry {
    fn new(
        session_telemetry: SessionTelemetry,
        auth_context: AuthRequestTelemetryContext,
        request_route_telemetry: RequestRouteTelemetry,
        auth_env_telemetry: AuthEnvTelemetry,
    ) -> Self {
        Self {
            session_telemetry,
            auth_context,
            request_route_telemetry,
            auth_env_telemetry,
        }
    }
}

impl RequestTelemetry for ApiTelemetry {
    fn on_request(
        &self,
        attempt: u64,
        status: Option<HttpStatusCode>,
        error: Option<&TransportError>,
        duration: Duration,
    ) {
        let error_message = error.map(telemetry_transport_error_message);
        let status = status.map(|s| s.as_u16());
        let debug = error
            .map(extract_response_debug_context)
            .unwrap_or_default();
        self.session_telemetry.record_api_request(
            attempt,
            status,
            error_message.as_deref(),
            duration,
            self.auth_context.auth_header_attached,
            self.auth_context.auth_header_name,
            self.auth_context.retry_after_unauthorized,
            self.auth_context.recovery_mode,
            self.auth_context.recovery_phase,
            self.request_route_telemetry.endpoint,
            debug.request_id.as_deref(),
            debug.cf_ray.as_deref(),
            debug.auth_error.as_deref(),
            debug.auth_error_code.as_deref(),
        );
        emit_feedback_request_tags_with_auth_env(
            &FeedbackRequestTags {
                endpoint: self.request_route_telemetry.endpoint,
                auth_header_attached: self.auth_context.auth_header_attached,
                auth_header_name: self.auth_context.auth_header_name,
                auth_mode: self.auth_context.auth_mode,
                auth_retry_after_unauthorized: Some(self.auth_context.retry_after_unauthorized),
                auth_recovery_mode: self.auth_context.recovery_mode,
                auth_recovery_phase: self.auth_context.recovery_phase,
                auth_connection_reused: None,
                auth_request_id: debug.request_id.as_deref(),
                auth_cf_ray: debug.cf_ray.as_deref(),
                auth_error: debug.auth_error.as_deref(),
                auth_error_code: debug.auth_error_code.as_deref(),
                auth_recovery_followup_success: self
                    .auth_context
                    .retry_after_unauthorized
                    .then_some(error.is_none()),
                auth_recovery_followup_status: self
                    .auth_context
                    .retry_after_unauthorized
                    .then_some(status)
                    .flatten(),
            },
            &self.auth_env_telemetry,
        );
    }
}

impl SseTelemetry for ApiTelemetry {
    fn on_sse_poll(
        &self,
        result: &std::result::Result<
            Option<std::result::Result<Event, EventStreamError<TransportError>>>,
            tokio::time::error::Elapsed,
        >,
        duration: Duration,
    ) {
        self.session_telemetry.log_sse_event(result, duration);
    }
}

impl WebsocketTelemetry for ApiTelemetry {
    fn on_ws_request(&self, duration: Duration, error: Option<&ApiError>, connection_reused: bool) {
        let error_message = error.map(telemetry_api_error_message);
        let status = error.and_then(api_error_http_status);
        let debug = error
            .map(extract_response_debug_context_from_api_error)
            .unwrap_or_default();
        self.session_telemetry.record_websocket_request(
            duration,
            error_message.as_deref(),
            connection_reused,
        );
        emit_feedback_request_tags_with_auth_env(
            &FeedbackRequestTags {
                endpoint: self.request_route_telemetry.endpoint,
                auth_header_attached: self.auth_context.auth_header_attached,
                auth_header_name: self.auth_context.auth_header_name,
                auth_mode: self.auth_context.auth_mode,
                auth_retry_after_unauthorized: Some(self.auth_context.retry_after_unauthorized),
                auth_recovery_mode: self.auth_context.recovery_mode,
                auth_recovery_phase: self.auth_context.recovery_phase,
                auth_connection_reused: Some(connection_reused),
                auth_request_id: debug.request_id.as_deref(),
                auth_cf_ray: debug.cf_ray.as_deref(),
                auth_error: debug.auth_error.as_deref(),
                auth_error_code: debug.auth_error_code.as_deref(),
                auth_recovery_followup_success: self
                    .auth_context
                    .retry_after_unauthorized
                    .then_some(error.is_none()),
                auth_recovery_followup_status: self
                    .auth_context
                    .retry_after_unauthorized
                    .then_some(status)
                    .flatten(),
            },
            &self.auth_env_telemetry,
        );
    }

    fn on_ws_event(
        &self,
        result: &std::result::Result<Option<std::result::Result<Message, Error>>, ApiError>,
        duration: Duration,
    ) {
        self.session_telemetry
            .record_websocket_event(result, duration);
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
