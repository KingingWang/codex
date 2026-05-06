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

### 使用方式

配置 `base_url` 指向你的内部 LLM 服务：

```toml
# ~/.codex/config.toml
[model_provider]
base_url = "https://your-internal-llm-service.example.com/v1"
```

或通过环境变量：

```shell
export CODEX_MODEL_PROVIDER_BASE_URL="https://your-internal-llm-service.example.com/v1"
```

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