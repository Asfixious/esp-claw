# User Guide

Framework 有三种主要使用方式：Host CLI、Rust library 和 ESP-IDF C ABI。它们共享 AgentSystem 行为，但配置、I/O 与 ownership 不同。

## Host CLI

先完成 [`getting-started.md`](getting-started.md) 的真实模型配置，然后运行：

```bash
cargo run -p claw-cli --bin claw-agent-chat
```

第一条普通消息会懒创建一个 persistent Session。CLI 会显示 Memory 目录、当前 Session、流式 reasoning/output、Tool result、permission request 和 usage。

### 命令

| 命令 | 作用 | 何时使用 |
| --- | --- | --- |
| `/append <message>` | 将输入排到当前 Session 后续 turn | 当前 turn 运行中仍希望排队 |
| `/interrupt` | 协作式中断当前前台 turn | 希望当前工作尽快收尾并停止 |
| `/cancel` | 取消当前 Session 的前台和后台工作 | 需要硬停止 |
| `/session new [persistent\|ephemeral]` | 创建并进入新 Session | 隔离不同任务 |
| `/session resume session-N` | 打开已有 Session | 恢复 persistent Session |
| `/session delete [session-N]` | 删除指定或当前 Session | 永久清理状态 |
| `/permissions <deny\|ask\|allow_all>` | 设置 Session permission level | 控制 Tool 风险策略 |
| `/reasoning_effort <low\|medium\|high\|ultra>` | 设置 reasoning effort | 调整下一 turn 的推理配置 |

直接输入空行或在 idle 状态按 Ctrl-C 会退出。Turn 运行中按 Ctrl-C 会先请求 cancel。

### 输出文件

| 路径 | 内容 |
| --- | --- |
| `crates/claw-cli/output/claw-agent-chat/` | Session、Transcript、Profile 与 Runtime 持久化 |
| `crates/claw-cli/simulator_log.log` | plain log |
| `crates/claw-cli/simulator_trace.log` | structured trace |

这些路径相对 crate 固定，不受启动 shell 的当前目录影响。

## Session 心智模型

Session 是长期容器，不等于一次问答。一次用户提交创建一个 Turn，一个 Turn 可以包含多个 LLM/Tool Iteration。

```text
Session
├── Turn 1 (user)
│   ├── Iteration 1: LLM -> Tool Call
│   └── Iteration 2: Tool Result -> final output
├── Turn 2 (user)
└── Turn 3 (detached completion wakes root)
```

- `persistent` Session 在 Runtime 重启后可恢复。
- `ephemeral` Session 只存在于当前进程。
- `append/submit` 创建或排队新的用户 Turn。
- `respond` 回答当前 Turn 内的 `InputRequested`，不能拿旧 request ID 新开 Turn。
- `TurnEnded` 不是 Session 终止；只有 `Closed` 终止 event stream。

## Permission request

当 policy 返回 `Ask`，Runtime 发出语义化 `InputRequested`。CLI 会将其显示为提示文本，并把下一条普通输入作为 `respond` 发送。

调用方不应把 permission request 当作普通 Agent output 后再 `submit`；这样会新开 Turn，并让原 request 保持等待或过期。

## Rust library

Rust 嵌入通常遵循：

1. 选择 `ClawFs`、HTTP、Timer、Thread、Executor 实现。
2. 构造 `AgentSystem` 和 Tool groups。
3. `link_api`。
4. `start_all`。
5. `new_session`、`open_session`。
6. 通过 `SessionControl` 提交和控制。
7. 持续消费 `SessionStream`。
8. 停止时 `stop_all`。

完整类型、错误与 ownership 见 [Rust API](api/rust-api.md)。

## ESP-IDF C ABI

C 应用通过 `claw_agent.h` 管理全局 Runtime。典型流程：

```text
claw_agent_init
  -> register/start C capabilities
  -> claw_agent_start
  -> session_create
  -> session_open
  -> session_submit/respond
  -> receive loop + event_free
  -> session_close/delete
  -> claw_agent_stop
  -> claw_agent_deinit
```

ESP-Claw 产品固件通常不直接在业务代码中重复这套 loop，而由 `cap_agent` 独占 Session stream，再把完整输出和结构事件送到 Event Router。

完整 C 示例与错误码见 [C API](api/c-api.md)。

## Tool 与 Skill

Tool group 可以注册但不默认出现在模型 context 中。模型先通过 `tool_search` 获取 group 摘要，再通过 `tool_load` 临时载入 schema。隐藏 schema 只节省 context，不提供安全隔离。

Skill 是带 frontmatter 的 `SKILL.md`。Runtime 扫描多个 root，前面的 root 优先；Skill activation 把文档内容作为一次性 context，而不是把任意脚本自动执行。

## 停止、恢复与删除

- `interrupt`：协作式停止前台工作，保留 Session。
- `cancel`：同时取消前后台工作，保留 Session identity。
- `close`：关闭当前 stream，Session 仍可重新打开。
- `delete`：删除 Session 和对应 durable state；这是不可逆操作。
- `stop_all`/`claw_agent_stop`：停止 Tool lifecycle，保留 Runtime 与 Sessions。
- deinit：释放 Runtime；下次 init 从持久化恢复 persistent Sessions。
