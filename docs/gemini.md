# Gemini native protocol

Configure a provider with `wire_api = "gemini"`. `base_url` includes the API
version or gateway prefix; Codex appends
`models/{model}:generateContent` (JSON), or
`models/{model}:streamGenerateContent?alt=sse` when `chat_stream = true`.

```toml
model_provider = "gemini"
model = "gemini-3-flash-preview"

[model_providers.gemini]
name = "Gemini"
base_url = "https://generativelanguage.googleapis.com/v1beta"
env_key = "GEMINI_API_KEY"
wire_api = "gemini"
chat_stream = true
```

Use your gateway's versioned URL instead when connecting through a proxy.
Set `model` to the exact model ID supported by your provider.
Keys resolved through `env_key`, command auth, or the existing credential chain
are sent as `x-goog-api-key`. Explicit `http_headers` / `env_http_headers`
with `x-goog-api-key` take precedence. An explicitly configured `Authorization`
header selects bearer authentication for OAuth or gateways.

Requests include `x-session-id`, `x-session-affinity`, `x-codex-window-id`, and
`x-parent-session-id` for child sessions, in addition to configured provider
headers. Configured query parameters are retained when adding `alt=sse`.

HTTP failures use the same retry and authentication recovery as Anthropic.
Malformed responses, upstream SSE errors, incomplete streams, and reasoning-only
responses enter the existing request/stream retry paths. Request and stream
retry settings retain their existing semantics.
Responses with a `continuationToken` resume from that checkpoint, including
`CONTINUATION` finishes and recoverable stream disconnects, without replaying
already received output.

Gemini's implicit prompt cache needs no OpenAI `promptCacheKey` or Anthropic
`cache_control`. Codex keeps the system prefix, sorted tool definitions, and
historical contents stable, and reports `cachedContentTokenCount` as cached input
tokens. A hit still depends on upstream model thresholds and cache lifetime;
Codex does not create or manage explicit `cachedContents` resources.

Tool calls, parallel tool results, inline base64 images, thinking, and structured
JSON output are converted to Gemini's native fields. Signed model parts
(`thoughtSignature`) are retained in session history for tool round trips and
resume. Gemini 3 uses `thinkingLevel`; Gemini 2.5 uses `thinkingBudget`.
Older Gemini models omit function-call IDs; unsigned imported Gemini 3 calls
use Google's documented signature-validator compatibility sentinel.
Remote image URLs and audio inputs are not supported by this adapter.

Focused verification (single-job compilation):

```sh
cargo test -p codex-api -p codex-model-provider-info -p codex-model-provider -p codex-core --lib gemini -j 1
cargo test -p codex-core --test all gemini -j 1
```
