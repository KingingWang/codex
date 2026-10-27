//! Native Gemini HTTP endpoint, using the same transport retry session as other
//! protocols. Authentication is rewritten *after* resolving the shared auth.

use std::sync::Arc;
use std::time::Duration;

use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::Request;
use http::HeaderMap;
use http::HeaderValue;
use http::Method;
use http::header::AUTHORIZATION;

use crate::auth::AuthError;
use crate::auth::AuthHeadersFuture;
use crate::auth::AuthProvider;
use crate::auth::AuthProviderFuture;
use crate::auth::SharedAuthProvider;
use crate::common::ResponseEvent;
use crate::common::ResponseStream;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::gemini_types::GeminiRequest;
use crate::provider::Provider;
use crate::sse::gemini::GeminiContinuation;
use crate::sse::gemini::GeminiEvents;
use crate::sse::gemini::parse_response;
use crate::sse::gemini::response_stream;
use crate::sse::gemini::spawn_gemini_stream;

struct GeminiAuth {
    inner: SharedAuthProvider,
    /// An explicitly configured Authorization header can select Google OAuth
    /// or a bearer-authenticated gateway instead of native API-key auth.
    preserve_bearer: bool,
}

fn rewrite_headers(headers: &mut HeaderMap, preserve_bearer: bool) {
    if let Some(key) = headers.get_mut("x-goog-api-key") {
        key.set_sensitive(true);
    }
    if preserve_bearer {
        return;
    }
    if let Some(value) = headers.get(AUTHORIZATION)
        && let Ok(value) = value.to_str()
        && let Some(token) = value.strip_prefix("Bearer ")
        && let Ok(mut key) = HeaderValue::from_str(token.trim())
    {
        key.set_sensitive(true);
        if !headers.contains_key("x-goog-api-key") {
            headers.insert("x-goog-api-key", key);
        }
        headers.remove(AUTHORIZATION);
    }
}

impl AuthProvider for GeminiAuth {
    fn add_auth_headers(&self, headers: &mut HeaderMap) -> Result<(), AuthError> {
        self.inner.add_auth_headers(headers)?;
        rewrite_headers(headers, self.preserve_bearer);
        Ok(())
    }

    fn resolve_auth_headers(&self) -> AuthHeadersFuture<'_> {
        Box::pin(async {
            let mut headers = self.inner.resolve_auth_headers().await?;
            rewrite_headers(&mut headers, self.preserve_bearer);
            Ok(headers)
        })
    }

    fn apply_auth(&self, request: Request) -> AuthProviderFuture<'_> {
        Box::pin(async move {
            let mut request = self.inner.apply_auth(request).await?;
            rewrite_headers(&mut request.headers, self.preserve_bearer);
            Ok(request)
        })
    }
}

pub struct GeminiClient<T: HttpTransport> {
    session: Arc<EndpointSession<T>>,
    stream: bool,
}

impl<T: HttpTransport + 'static> GeminiClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider, stream: bool) -> Self {
        let preserve_bearer = provider.headers.contains_key(AUTHORIZATION);
        Self {
            session: Arc::new(EndpointSession::new(
                transport,
                provider,
                Arc::new(GeminiAuth {
                    inner: auth,
                    preserve_bearer,
                }),
            )),
            stream,
        }
    }

    /// Base URL includes the API version (or a gateway's versioned prefix).
    pub fn path(model: &str, stream: bool) -> Result<String, ApiError> {
        let model = model.strip_prefix("models/").unwrap_or(model);
        if model.is_empty() || model.contains(['/', '?', '#']) || matches!(model, "." | "..") {
            return Err(ApiError::InvalidRequest {
                message: "invalid Gemini model ID".into(),
            });
        }
        let mut url = url::Url::parse("https://gemini.invalid/models/").map_err(|err| {
            ApiError::InvalidRequest {
                message: err.to_string(),
            }
        })?;
        url.path_segments_mut()
            .map_err(|_| ApiError::InvalidRequest {
                message: "invalid Gemini model path".into(),
            })?
            .pop_if_empty()
            .push(model);
        let method = if stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        Ok(format!("{}:{method}", url.path().trim_start_matches('/')))
    }

    pub async fn request(
        &self,
        mut request: GeminiRequest,
        headers: HeaderMap,
    ) -> Result<ResponseStream, ApiError> {
        let path = Self::path(&request.model, self.stream)?;
        let names = std::mem::take(&mut request.tool_name_map);
        let mut body = serde_json::to_value(request).map_err(|error| ApiError::InvalidRequest {
            message: format!("failed to encode Gemini request: {error}"),
        })?;
        if self.stream {
            let encoded =
                EncodedJsonBody::encode(&body).map_err(|error| ApiError::InvalidRequest {
                    message: format!("failed to encode Gemini request: {error}"),
                })?;
            let response = self
                .session
                .stream_encoded_json_with(
                    Method::POST,
                    &path,
                    headers.clone(),
                    Some(encoded),
                    configure_stream_request,
                )
                .await?;
            let session = Arc::clone(&self.session);
            let continuation: GeminiContinuation = Arc::new(move |token| {
                let session = Arc::clone(&session);
                let headers = headers.clone();
                let path = path.clone();
                let mut body = body.clone();
                Box::pin(async move {
                    body["continuationToken"] = serde_json::Value::String(token);
                    let body = EncodedJsonBody::encode(&body)
                        .map_err(|error| ApiError::Stream(error.to_string()))?;
                    session
                        .stream_encoded_json_with(
                            Method::POST,
                            &path,
                            headers,
                            Some(body),
                            configure_stream_request,
                        )
                        .await
                })
            });
            Ok(spawn_gemini_stream(
                response,
                self.session.provider().stream_idle_timeout,
                names,
                Some(continuation),
            ))
        } else {
            let mut state = GeminiEvents::new(names);
            let mut events = vec![ResponseEvent::Created { response_id: None }];
            loop {
                let response = self
                    .session
                    .execute_with(
                        Method::POST,
                        &path,
                        headers.clone(),
                        Some(body.clone()),
                        |request| request.timeout = Some(Duration::from_secs(600)),
                    )
                    .await?;
                events.extend(state.push(parse_response(&response.body)?)?);
                if let Some(token) = state.next_request() {
                    body["continuationToken"] = serde_json::Value::String(token);
                    continue;
                }
                let request_id = response
                    .headers
                    .get("x-request-id")
                    .or_else(|| response.headers.get("x-goog-request-id"))
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                events.extend(state.finish()?);
                return Ok(response_stream(events, request_id));
            }
        }
    }
}

fn configure_stream_request(request: &mut Request) {
    request.timeout = Some(Duration::from_secs(600));
    // Add alt=sse without corrupting configured query parameters.
    if let Ok(mut url) = url::Url::parse(&request.url) {
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(name, _)| name != "alt")
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        url.query_pairs_mut()
            .extend_pairs(pairs)
            .append_pair("alt", "sse");
        request.url = url.into();
    }
}

#[cfg(test)]
#[path = "gemini_tests.rs"]
mod tests;
