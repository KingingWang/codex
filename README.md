# Codex CLI - Internal/Private Deployment

> 本仓库基于 [OpenAI Codex CLI](https://github.com/openai/codex) 修改，专为企业内部和隐私敏感场景使用。

## 与官方仓库的区别

本仓库对官方 Codex CLI 进行了以下修改，确保**用户数据不会外发**：

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

### 安全说明

- **零数据外发**：所有可能向外部发送用户代码、对话内容、系统信息的通道均已关闭
- **本地模型支持**：内置 Ollama 和 LM Studio 提供商，可完全离线运行
- **可信 LLM 提供商**：可配置 `base_url` 指向企业内部部署的 LLM 服务
- **沙箱隔离**：保留原有的 bwrap/Linux 沙箱机制，限制子进程网络访问

---

## 配置指南

配置文件位于 `~/.codex/config.toml`。以下是一份完整的参考配置，涵盖了 Chat Completions API 协议切换、流式/非流式模式等常用选项。

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

# API 基础 URL，必须指向 OpenAI 兼容的端点
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
#
# 大多数第三方兼容端点（如 vLLM、Ollama、OneAPI、LiteLLM 等）
# 只支持 Chat Completions API，请设置为 "chat"。
wire_api = "chat"

# ----------------------------------------------------------
# 流式模式：chat_stream
# ----------------------------------------------------------
# 仅在 wire_api = "chat" 时生效。
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

#### 场景 4：本地 Ollama

```toml
model_provider = "ollama"
model = "qwen2.5-coder:32b"
```

#### 场景 5：本地 LM Studio

```toml
model_provider = "lmstudio"
model = "your-downloaded-model"
```

### 环境变量方式

你也可以通过环境变量快速配置，无需修改配置文件：

```shell
# 设置 API 基础 URL
export CODEX_MODEL_PROVIDER_BASE_URL="https://your-internal-llm.example.com/v1"

# 设置 API Key（如果 provider 配置了 env_key）
export MY_LLM_API_KEY="sk-xxxxxxxxxxxxxxxxxxxxxxxx"

# 运行
codex
```

### 配置字段速查表

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `model_provider` | string | — | 选择使用的提供商 ID |
| `model` | string | — | 模型名称 |
| `base_url` | string | — | API 基础 URL（必须） |
| `env_key` | string | — | 存储 API Key 的环境变量名 |
| `wire_api` | string | `"responses"` | 协议：`"responses"` 或 `"chat"` |
| `chat_stream` | bool | `false` | 是否启用 SSE 流式（仅 `chat` 协议有效） |
| `request_max_retries` | int | `4` | HTTP 请求最大重试次数 |
| `stream_max_retries` | int | `5` | 流式重连最大次数 |
| `stream_idle_timeout_ms` | int | `300000` | 流式空闲超时（毫秒） |
| `http_headers` | object | — | 自定义 HTTP 请求头 |
| `query_params` | object | — | URL 附加查询参数 |

---

## 变更记录

基于官方仓库的修改提交（作者：kingingwang）：

- 禁用内部部署的外部服务
- Chat Completions API 类型及 SSE 流式传输支持
- /model 命令及 end_turn 字段
- User-Agent 伪装及外部 URL 清除

---

## 官方文档

完整的 Codex CLI 使用文档请参考官方仓库：[https://github.com/openai/codex](https://github.com/openai/codex)

---
