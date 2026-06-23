use crate::auth::SharedAuthProvider;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::telemetry::run_with_request_telemetry;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::Request;
use codex_client::RequestBody;
use codex_client::RequestTelemetry;
use codex_client::Response;
use codex_client::StreamResponse;
use codex_client::TransportError;
use http::HeaderMap;
use http::Method;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::instrument;
use tracing::warn;

pub(crate) struct EndpointSession<T: HttpTransport> {
    transport: T,
    provider: Provider,
    auth: SharedAuthProvider,
    request_telemetry: Option<Arc<dyn RequestTelemetry>>,
}

impl<T: HttpTransport> EndpointSession<T> {
    pub(crate) fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            transport,
            provider,
            auth,
            request_telemetry: None,
        }
    }

    pub(crate) fn with_request_telemetry(
        mut self,
        request: Option<Arc<dyn RequestTelemetry>>,
    ) -> Self {
        self.request_telemetry = request;
        self
    }

    pub(crate) fn provider(&self) -> &Provider {
        &self.provider
    }

    fn make_request(
        &self,
        method: &Method,
        path: &str,
        extra_headers: &HeaderMap,
        body: Option<&RequestBody>,
    ) -> Request {
        let mut req = self.provider.build_request(method.clone(), path);
        req.headers.extend(extra_headers.clone());
        if let Some(body) = body {
            req.body = Some(body.clone());
        }
        req
    }

    pub(crate) async fn execute(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<Value>,
    ) -> Result<Response, ApiError> {
        self.execute_with(method, path, extra_headers, body, |_| {})
            .await
    }

    #[instrument(
        name = "endpoint_session.execute_with",
        level = "info",
        skip_all,
        fields(http.method = %method, api.path = path)
    )]
    pub(crate) async fn execute_with<C>(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<Value>,
        configure: C,
    ) -> Result<Response, ApiError>
    where
        C: Fn(&mut Request),
    {
        let body = body.map(RequestBody::Json);
        let make_request = || {
            let mut req = self.make_request(&method, path, &extra_headers, body.as_ref());
            configure(&mut req);
            req
        };
        let base_delay = Duration::from_secs(5);
        let max_delay = Duration::from_secs(600);
        let mut retry_count: u32 = 0;

        loop {
            let result = run_with_request_telemetry(
                self.provider.retry.to_policy(),
                self.request_telemetry.clone(),
                make_request,
                |req| {
                    let auth = self.auth.clone();
                    let transport = &self.transport;
                    async move {
                        let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                        transport.execute(req).await
                    }
                },
            )
            .await;

            match result {
                Ok(response) => return Ok(response),
                Err(transport_err @ TransportError::Build(_)) => {
                    return Err(ApiError::from(transport_err));
                }
                Err(transport_err) => {
                    let api_err = ApiError::from(transport_err);
                    let multiplier = 1u32.checked_shl(retry_count.min(20)).unwrap_or(u32::MAX);
                    let delay = std::cmp::min(base_delay.saturating_mul(multiplier), max_delay);
                    warn!(
                        retry_count,
                        delay_ms = delay.as_millis(),
                        error = %api_err,
                        "Request to {path} failed, retrying after backoff"
                    );
                    sleep(delay).await;
                    retry_count += 1;
                }
            }
        }
    }

    #[instrument(
        name = "endpoint_session.stream_encoded_json_with",
        level = "info",
        skip_all,
        fields(http.method = %method, api.path = path)
    )]
    pub(crate) async fn stream_encoded_json_with<C>(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<EncodedJsonBody>,
        configure: C,
    ) -> Result<StreamResponse, ApiError>
    where
        C: Fn(&mut Request),
    {
        let body = body.map(RequestBody::EncodedJson);
        let mut request = self.make_request(&method, path, &extra_headers, body.as_ref());
        configure(&mut request);
        let request = request.into_prepared().map_err(TransportError::Build)?;
        let make_request = || request.clone();
        let base_delay = Duration::from_secs(5);
        let max_delay = Duration::from_secs(600);
        let mut retry_count: u32 = 0;

        loop {
            let result = run_with_request_telemetry(
                self.provider.retry.to_policy(),
                self.request_telemetry.clone(),
                make_request,
                |req| {
                    let auth = self.auth.clone();
                    let transport = &self.transport;
                    async move {
                        let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                        transport.stream(req).await
                    }
                },
            )
            .await;

            match result {
                Ok(stream) => return Ok(stream),
                Err(transport_err @ TransportError::Build(_)) => {
                    return Err(ApiError::from(transport_err));
                }
                Err(transport_err) => {
                    let api_err = ApiError::from(transport_err);
                    let multiplier = 1u32.checked_shl(retry_count.min(20)).unwrap_or(u32::MAX);
                    let delay = std::cmp::min(base_delay.saturating_mul(multiplier), max_delay);
                    warn!(
                        retry_count,
                        delay_ms = delay.as_millis(),
                        error = %api_err,
                        "Stream request to {path} failed, retrying after backoff"
                    );
                    sleep(delay).await;
                    retry_count += 1;
                }
            }
        }
    }
}
