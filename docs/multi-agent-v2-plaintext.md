# Multi-Agent V2 plaintext messages

Multi-Agent V2 uses the `agent_tools` namespace by default. The function names
(`spawn_agent`, `send_message`, `followup_task`, and the other agent controls)
are unchanged. Existing `tool_namespace = "collaboration"` configurations
automatically use `agent_tools`, because OpenAI reserves `collaboration` and
requires encrypted message parameters there. Other custom namespaces are preserved.

The three messaging tools do not advertise `encrypted` parameter annotations.
Encryption annotations from model-catalog parameter overrides are also removed,
including annotations in nested schemas. Initial tasks, queued messages, and
follow-up tasks are delivered as plaintext inter-agent messages, both for direct
calls and Code Mode calls. This allows other providers to read the messages
without depending on OpenAI's server-side decryption.

Custom integrations should use the namespace in the current tool definitions
rather than hard-code `collaboration`. Model reasoning and compaction encryption,
legacy encrypted history, and Multi-Agent V1 are not changed. Previously encrypted
messages are not decrypted by this change.

## Verification

- The configured gateway and `gpt-6.1-sol` returned the exact plaintext test
  message for all three messaging tools under `agent_tools`.
- 69 V2-focused unit tests passed. The broader tool unit run passed 505 tests
  with one existing ignored test, excluding the shell-snapshot runtime group
  because the local login scripts change its expected PATH.
- 76 targeted integration cases passed, including WebSocket Code Mode spawning,
  model-catalog overrides, mid-turn model changes, and agent lifecycle tests.
- 108 scenario cases passed again with snapshot updating disabled. This run
  excluded credential masking, mailbox preemption, settled Code Mode helpers,
  and direct MCP tool discovery.

The complete test suite is not green in this environment. Building the current
Code Mode host fails because its V8 prebuilt binding URL returns HTTP 404.
The installed 0.160.1 host was used for WebSocket verification; its gRPC session
opening fails and it lacks the newer `as_settled` helper. Additional failures
include restricted bubblewrap namespaces, mailbox WebSocket timeouts, and a
direct MCP discovery assertion. The full formatter check also reports an
existing import-order issue in `quota_exceeded.rs`; changed Rust files pass
their individual formatter checks.
