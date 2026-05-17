# Codex CLI - Internal/Private Deployment

> 本仓库基于 [OpenAI Codex CLI](https://github.com/openai/codex) 修改，专为企业内部和隐私敏感场景使用。

## 与官方仓库的区别

本仓库对官方 Codex CLI 进行了以下修改，默认关闭官方遥测、登录、更新检查和内置云端模型入口。除非你显式配置并选择某个远端 LLM provider，否则不会把对话或代码发送到外部模型服务：

### 已禁用的外部服务

| 服务 | 说明 |
|------|------|
| Sentry 崩溃/反馈上报 | DSN 已清空，不再向外部发送崩溃日志和用户反馈 |
| OpenAI OAuth 登录 | 认证端点已清空，`codex login` 不会连接 OpenAI |
| OpenAI Agent Identity | 认证 API 端点已清空 |
| 版本更新检查 | 启动时不再检查 GitHub/npm/Homebrew 更新 |
| 遥测/分析 | Analytics 默认关闭，不会发送使用数据 |
| Cloud Tasks | 默认关闭，不连接外部任务服务 |
| Statsig 指标 | 已硬编码禁用，不发送任何指标数据 |
| 桌面端自动安装 | Mac DMG 和 Windows 安装包下载链接已清空 |
| 内置 OpenAI/Bedrock 提供商 | 已移除，仅保留 Ollama 和 LM Studio（本地） |
| OpenTelemetry Statsig 导出 | 已硬编码映射为 None |

### User-Agent 伪装

HTTP 请求的 User-Agent 已修改为 `RooCode/3.51.1`，不再暴露 Codex 版本号、操作系统架构等信息。

### 安全边界

- **默认不连接官方云服务**：OpenAI 登录、遥测、更新检查、Cloud Tasks、Statsig 等官方外部通道已禁用或清空。
- **模型请求由 provider 决定**：如果选择 Ollama / LM Studio，本地模型请求可完全离线；如果配置 Anthropic、OpenAI 兼容网关或其他远端 `base_url`，对话、代码片段、工具结果和图片会发送到该 provider。
- **可信 LLM 提供商**：隐私敏感场景建议把 `base_url` 指向企业内部部署的 LLM 服务，而不是公网模型服务。
- **沙箱隔离**：保留原有的 bwrap/Linux 沙箱机制，限制子进程网络访问。

---

## 配置指南

配置文件位于 `~/.codex/config.toml`。以下是一份完整的参考配置，涵盖了 Responses API、Chat Completions API、Anthropic Messages API 协议切换、流式/非流式模式等常用选项。

### 参考配置文件

```toml
# ============================================================
# ~/.codex/config.toml — 参考配置（请根据实际情况修改）
# ============================================================

# 全局默认模型（可选）
model = "gpt-4o"

# 选择使用哪个 provider（对应下方 model_providers 的 key）
model_provider = "my-provider"

# ============================================================
# 自定义模型提供商配置
# ============================================================
[model_providers.my-provider]
# 提供商显示名称（可选）
name = "My Internal LLM"

# API 基础 URL。OpenAI Responses / Chat 兼容端点通常以 /v1 结尾；
# Anthropic Messages API 使用 provider 根地址，不要追加 /v1。
base_url = "https://your-internal-llm.example.com/v1"

# API Key 来源：通过环境变量获取（推荐）
env_key = "MY_LLM_API_KEY"
# 可选：当环境变量未设置时给用户的提示
env_key_instructions = "请设置 MY_LLM_API_KEY 环境变量为你的 API Key"

# 或者直接在配置中写死 API Key（不推荐，有安全风险）
# experimental_bearer_token = "sk-xxxxxxxxxxxxxxxxxxxxxxxx"

# ----------------------------------------------------------
# 协议选择：wire_api
# ----------------------------------------------------------
# "responses"（默认）— 使用 OpenAI Responses API（/v1/responses）
# "chat"             — 使用 Chat Completions API（/v1/chat/completions）
# "anthropic"        — 使用 Anthropic Messages API（/v1/messages）
#
# 大多数第三方兼容端点（如 vLLM、Ollama、OneAPI、LiteLLM 等）
# 只支持 Chat Completions API，请设置为 "chat"。
# Anthropic 或 Anthropic 兼容网关请设置为 "anthropic"。
wire_api = "chat"

# ----------------------------------------------------------
# 流式模式：chat_stream
# ----------------------------------------------------------
# 在 wire_api = "chat" 或 "anthropic" 时生效。
# false（默认）— 非流式请求，等待完整响应后一次性返回
# true         — 使用 SSE 流式传输，逐 token 实时输出
#
# 如果你的端点支持流式输出（推荐），设置为 true 以获得更好的交互体验。
chat_stream = true

# ----------------------------------------------------------
# 可选：自定义 HTTP 请求头
# ----------------------------------------------------------
# http_headers = { "X-Custom-Header" = "value" }
# 从环境变量读取请求头（更安全）
# env_http_headers = { "Authorization" = "MY_AUTH_HEADER" }

# ----------------------------------------------------------
# 可选：重试与超时配置
# ----------------------------------------------------------
# HTTP 请求最大重试次数（默认 4）
# request_max_retries = 4
# 流式连接断开后最大重连次数（默认 5）
# stream_max_retries = 5
# 流式响应空闲超时时间，毫秒（默认 300000 = 5 分钟）
# stream_idle_timeout_ms = 300000

# ----------------------------------------------------------
# 可选：URL 查询参数
# ----------------------------------------------------------
# query_params = { "timeout" = "600" }

# ============================================================
# 内置本地提供商（开箱即用，无需额外配置）
# ============================================================
# Ollama — 默认连接 http://localhost:11434/v1
# LM Studio — 默认连接 http://localhost:1234/v1
#
# 使用方式：
#   codex --local-provider ollama --model qwen2.5-coder:32b
#   codex --local-provider lmstudio --model your-model
#
# 或在配置中指定：
#   model_provider = "ollama"
#   model_provider = "lmstudio"

# ============================================================
# 其他常用配置项
# ============================================================

# 上下文窗口大小（tokens，可选）
# model_context_window = 128000

# 推理强度（部分模型支持）：low | medium | high | xhigh
# model_reasoning_effort = "medium"

# ----------------------------------------------------------
# 可选：动态上下文脚本
# ----------------------------------------------------------
# 指定一个可执行脚本路径，每次采样请求前执行，
# 将脚本的 stdout 输出作为 developer 消息追加到 prompt 末尾。
# 输出不会持久化到对话历史中。
#
# dynamic_context_script = "/path/to/your/script.sh"
#
# 脚本执行超时时间（秒），默认 5 秒。
# dynamic_context_script_timeout_secs = 10

# 分析/遥测（已默认关闭）
[analytics]
enabled = false
```

### 常见场景配置示例

#### 场景 1：Chat Completions API + 非流式（默认）

适用于不支持 SSE 流式的端点，或需要完整响应后再处理的场景：

```toml
model_provider = "my-provider"

[model_providers.my-provider]
base_url = "https://your-internal-llm.example.com/v1"
env_key = "MY_LLM_API_KEY"
wire_api = "chat"
chat_stream = false    # 非流式（默认值，可省略）
```

#### 场景 2：Chat Completions API + SSE 流式

适用于支持流式输出的端点（推荐，交互体验更好）：

```toml
model_provider = "my-provider"

[model_providers.my-provider]
base_url = "https://your-internal-llm.example.com/v1"
env_key = "MY_LLM_API_KEY"
wire_api = "chat"
chat_stream = true     # 启用 SSE 流式
```

#### 场景 3：Responses API（OpenAI 兼容端点）

仅适用于完整支持 OpenAI Responses API (`/v1/responses`) 的端点：

```toml
model_provider = "my-provider"

[model_providers.my-provider]
base_url = "https://your-internal-llm.example.com/v1"
env_key = "MY_LLM_API_KEY"
wire_api = "responses"  # 使用 Responses API
```

#### 场景 4：Anthropic Messages API

适用于 Anthropic 官方 API 或兼容 Anthropic Messages API (`/v1/messages`) 的内部网关。认证仍可使用 `env_key`；客户端会把 `Authorization: Bearer <token>` 自动改写为 Anthropic 需要的 `x-api-key: <token>`，并补充 `anthropic-version` 请求头。

```toml
model_provider = "anthropic"
model = "claude-sonnet-4-5"

[model_providers.anthropic]
name = "Anthropic"
base_url = "https://api.anthropic.com"
env_key = "ANTHROPIC_API_KEY"
wire_api = "anthropic"
chat_stream = true  # 可选：Anthropic SSE 流式输出
```

#### 场景 5：Anthropic 兼容内部网关

```toml
model_provider = "company-claude"
model = "claude-sonnet-4-5"

[model_providers.company-claude]
name = "Company Claude Gateway"
base_url = "https://your-internal-anthropic-gateway.example.com"
env_key = "COMPANY_CLAUDE_API_KEY"
wire_api = "anthropic"
chat_stream = true
# 可选：如果网关要求额外请求头
# http_headers = { "X-Project" = "codex" }
```

#### 场景 6：本地 Ollama

```toml
model_provider = "ollama"
model = "qwen2.5-coder:32b"
```

#### 场景 7：本地 LM Studio

```toml
model_provider = "lmstudio"
model = "your-downloaded-model"
```

### 环境变量方式

API Key 推荐通过环境变量提供；provider 本身仍建议写在 `~/.codex/config.toml`，或用一次性的 `-c` 覆盖传入：

```shell
# 方式 A：config.toml 中已经配置 env_key = "MY_LLM_API_KEY"
export MY_LLM_API_KEY="sk-xxxxxxxxxxxxxxxxxxxxxxxx"
codex

# 方式 B：不改配置文件，临时指定 provider
export MY_LLM_API_KEY="sk-xxxxxxxxxxxxxxxxxxxxxxxx"
codex \
  -c model_provider="my-provider" \
  -c 'model_providers.my-provider={ name = "My Internal LLM", base_url = "https://your-internal-llm.example.com/v1", env_key = "MY_LLM_API_KEY", wire_api = "chat", chat_stream = true }'
```

本地 OSS provider 的地址可用 `CODEX_OSS_BASE_URL` 覆盖，例如：

```shell
export CODEX_OSS_BASE_URL="http://localhost:11434/v1"
codex --local-provider ollama --model qwen2.5-coder:32b
```

### 配置字段速查表

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `model_provider` | string | — | 选择使用的提供商 ID |
| `model` | string | — | 模型名称 |
| `base_url` | string | — | API 基础 URL；Responses/Chat 通常填到 `/v1`，Anthropic 填 provider 根地址 |
| `env_key` | string | — | 存储 API Key 的环境变量名 |
| `wire_api` | string | `"responses"` | 协议：`"responses"`、`"chat"` 或 `"anthropic"` |
| `chat_stream` | bool | `false` | 是否启用 SSE 流式（`chat` / `anthropic` 协议有效） |
| `request_max_retries` | int | `4` | HTTP 请求最大重试次数 |
| `stream_max_retries` | int | `5` | 流式重连最大次数 |
| `stream_idle_timeout_ms` | int | `300000` | 流式空闲超时（毫秒） |
| `http_headers` | object | — | 自定义 HTTP 请求头 |
| `query_params` | object | — | URL 附加查询参数 |
| `dynamic_context_script` | string | — | 动态上下文脚本路径，每次请求前执行，输出追加到 prompt |
| `dynamic_context_script_timeout_secs` | int | `5` | 动态上下文脚本执行超时时间（秒） |

---

## Anthropic 支持说明

近期提交新增了 Anthropic Messages API 适配，可通过 `wire_api = "anthropic"` 使用。注意：选择公网 Anthropic 官方 API 会把模型请求发送给 Anthropic；隐私敏感场景应使用可信内部 Anthropic 兼容网关。

- `base_url` 填 provider 根地址（例如 `https://api.anthropic.com`），客户端请求路径为 `/v1/messages`，同时支持非流式和 SSE 流式响应。
- 自动处理 Anthropic 认证头：把现有 bearer token 改写为 `x-api-key`，并补充 `anthropic-version: 2023-06-01`。
- 默认补充 `anthropic-beta: prompt-caching-2024-07-31`，便于直连 Anthropic、Bedrock 或兼容网关时稳定触发 prompt caching。
- Prompt caching 会在系统提示、工具定义和最近用户消息上放置缓存断点，控制在 Anthropic 限制的 4 个 breakpoint 内。
- 支持工具调用、工具结果、reasoning/thinking 内容，以及用户消息/工具输出中的图片内容转换。
- Anthropic 返回的 response id 可为空，客户端会兼容此类网关响应。

### Anthropic 请求调试 Dump

排查 prompt cache 命中率或网关兼容问题时，可设置环境变量导出实际发送给 Anthropic 的 JSON 请求体：

```shell
export CODEX_DEBUG_ANTHROPIC_DUMP_DIR=/tmp/codex-anthropic-dumps
codex
```

每次请求会写入类似 `anthropic-stream-<unix_nanos>.json` 或 `anthropic-nonstream-<unix_nanos>.json` 的文件，可对比连续 turn 的前缀是否发生漂移。注意 dump 中可能包含提示词、代码片段、图片 base64 和工具参数，请只写入受信任目录并按敏感数据处理。

---

## 变更记录

基于官方仓库的修改提交（作者：kingingwang）：

- 禁用内部部署的外部服务
- Chat Completions API 类型及 SSE 流式传输支持
- Anthropic Messages API 客户端、SSE/非流式支持、prompt caching、请求 dump 调试
- 工具输出拆分与图片内容传递支持
- /model 命令及 end_turn 字段
- User-Agent 伪装及外部 URL 清除

---

## 官方文档

本 fork 已禁用或修改部分官方云端能力，配置行为以本文档和本仓库代码为准。其他通用 Codex CLI 使用方式可参考官方仓库：[https://github.com/openai/codex](https://github.com/openai/codex)

---
