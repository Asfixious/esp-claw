# Rust API

Rust 应用的首选入口是 `claw_agent::AgentSystem<Filesystem, Http, Timer>`。类型参数记录 Runtime 使用的平台 backend，Thread 与 Executor 则在构造函数上选择。

## 最小生命周期

```text
AgentSystem::new / with_tool_groups
  -> link_api
  -> start_all
  -> new_session
  -> open_session
  -> SessionControl + SessionStream
  -> stop_all
```

可运行代码见 [`tool_submit_loop.rs`](../../crates/claw-agent/examples/tool_submit_loop.rs)。它使用 `MemFs`、scripted HTTP、`ImmediateTimer`、`StdThread` 和 `TokioExecutor`，不需要网络。

## `AgentSystem`

| API | 作用 | 重要错误/约束 |
| --- | --- | --- |
| `new` | 创建空 Tool registry 的 Runtime | FS cleanup、persistence 或 worker 构造可失败 |
| `with_tool_groups` | 构造时注册 Tool groups | group/name/schema validation 可失败 |
| `link_api` | 按 `ApiPurpose` 绑定 LLM | config 必须完整合法；turn boundary 生效 |
| `start_all` | 启动 Tool registry | Session 操作应在此之后开始 |
| `stop_all` | 停止 Tool registry | 保留 AgentSystem 与 durable state |
| `enable_tool` / `disable_tool` | 修改 registry override | Tool 必须存在 |
| `new_session` | 创建 persistent/ephemeral Session | worker 必须可用 |
| `list_sessions` | 列出 live Session IDs | 返回当前进程可见 Sessions |
| `open_session` | 获得 control + stream | 同一 Session 不能重复打开 |
| `delete_session` | 删除 Session state | 删除是永久操作 |

### Ownership

- `AgentSystem` 拥有 Tool registry、Runtime worker handle 和 persistence owner。
- `open_session` 返回 `SessionControl` 与唯一公开的 `SessionStream` read side。
- `SessionControl` 可在等待 stream 时用于提交控制命令。
- Drop stream 会关闭该连接，但不等于删除 Session。
- Application 负责保证 backend 的生命周期长于 Runtime；构造函数通常接管具体 backend 或共享 handle。

## Session API

`SessionControl` 提供 async 方法：

| 方法 | 语义 |
| --- | --- |
| `append(Message)` | 排队一个新的用户 Turn |
| `respond(InputRequestId, Message)` | 恢复当前等待输入的 Turn |
| `interrupt()` | 协作式终止当前前台 Turn |
| `cancel()` | 取消 Session 前台与后台工作 |
| `close()` | 关闭当前 Session connection |
| `set_permission_level()` | 修改 Session permission policy |
| `set_reasoning_effort()` | 修改后续 Turn 的 reasoning effort |

所有方法都可能返回 `SessionControlError`，包括 worker stopped、Session closed、not awaiting input 和 request ID mismatch。不要把 control command 当作 fire-and-forget；应处理返回值。

## Event stream

`SessionStream` 实现 `Stream<Item = Result<SessionEvent, SessionError>>`。事件分三层：

```text
SessionEvent
├── Turn(TurnEvent)
│   ├── Started
│   ├── InputRequested
│   ├── Iteration(IterationEvent)
│   ├── EffectOutput
│   ├── Error
│   └── Ended
├── Error
└── Closed
```

Iteration 内的 reasoning、output 和 Tool result 使用 `StreamPart::Delta` 与 `StreamPart::End`。调用方应按事件边界渲染，不要依赖字符串为空或下一个 event 猜测结束。

推荐消费规则：

- 收到 `InputRequested` 后保存 request ID，并通过 `respond` 回复。
- 收到 `TurnEvent::Ended` 只结束本次 UI/render scope，继续保留 stream。
- 收到 `SessionEvent::Closed` 后停止读取。
- Stream item 的 `Err` 是可观察错误，不要静默丢弃。
- Subagent 内部 iteration 不直接暴露；detached completion 通过 root Turn 表现。

## 注册 Tool

一个 Tool 至少实现 `ToolSpec` 和一种 handler：

```rust
use claw_agent::tools::{
    Action, RiskClass, SyncToolHandler, ToolInvocation, ToolOutput,
    ToolResult, ToolSpec,
};

struct ReadSensor;

impl ToolSpec for ReadSensor {
    fn name(&self) -> &str {
        "read_sensor"
    }

    fn schema(&self) -> &str {
        r#"{"type":"function","function":{"name":"read_sensor","parameters":{"type":"object","properties":{}}}}"#
    }

    fn classify(&self, _call: &ToolInvocation) -> Action {
        Action::new("read_sensor", RiskClass::Low)
    }
}

impl SyncToolHandler for ReadSensor {
    fn invoke(&self, _call: &ToolInvocation) -> ToolResult<ToolOutput> {
        Ok(ToolOutput {
            content: "23.5 C".into(),
            ok: true,
        })
    }
}
```

用 `Tool::from_sync(ReadSensor)` 放入 `ToolGroup`，再传给 `with_tool_groups`。Schema 必须描述与 handler 实际接受的参数；`arguments_value()` 的解析错误应原样返回。

### Tool ownership 与并发

- Handler 是 `Send + Sync` 的已注册实现。
- `ToolRegistry` 管全局启停，`ToolSet` 管单 Agent 可见性。
- `ToolRunner::run` 返回 join handle 和可选 detached handle。
- `concurrent()` 只声明同类调用能否并行，不免除 handler 内部同步责任。
- Tool 不应持有跨 await 的 registry/session lock。

## LLM API

`ClawApiConfig::new(backend, api_key, model, base_url)` 创建配置。`ClawApi` 是 blocking client；`ClawApiAsync` 是 async/streaming client。

主要 request：

- `ChatRequest`
- `ChatJsonRequest`
- `MediaRequest`

每次调用携带自己的 `RetryPolicy`。Abort、非法 URL/body 和普通 4xx 不重试；网络错误、408、429、5xx 可重试。更完整示例见 `crates/claw-api/README.md`。

## Memory API

| 类型 | 责任 |
| --- | --- |
| `TranscriptStore` | 逐 Turn append-only 原始记录 |
| `ProfileStore` | soul/identity/user 文档 |
| `LongTermMemory` | durable facts、tags、keywords |
| `Compactor` | 把 message window 变成 summary 的 seam |

`TurnHandle` 在正常 drop 时提交 Turn；`abandon()` 放弃未完成 Turn。不要在持有 message handle 时同时开启不兼容的 message section。

## Platform backends

新增平台时实现 `claw-interface` traits：

- `ClawFs` / `ClawFile`
- `ClawHttp` / `StreamingHttp`
- `ClawThread`
- `ClawExecutor`
- `ClawTimer`

Domain crates 不应检测具体 OS/芯片。Host implementations 和 test doubles 由 `claw-interface` features 提供，ESP-IDF implementations 位于 `claw-sys`。

## Error 与 panic

- 处理所有返回的 `Result`；不要用 `unwrap()` 替代产品错误策略。
- Library error 应保留 source chain。
- Cancellation/interrupt 是正常控制流，不应被包装成无法识别的字符串。
- `panic = "abort"` 用于设备 Release，因此 panic 没有 unwind cleanup；预期失败必须在 panic 前被建模为 Result。

## 公开 API 变更

修改 public item 后运行：

```bash
./update-public-api-snapshots.sh
git diff -- snapshots/
```

同时更新 rustdoc、本页、Example 和 migration note。Snapshot 变化不是自动批准。
