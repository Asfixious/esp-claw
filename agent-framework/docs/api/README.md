# API 文档

Framework 有两个公开入口：Rust `claw-agent` API 和 C `claw_agent.h` ABI。下层 crates 也有公开类型，但应用应优先从最上层入口开始，只有需要替换子系统时才直接依赖下层 crate。

## 选择入口

| 调用方 | 推荐入口 | 文档 |
| --- | --- | --- |
| Rust Host/Server/测试应用 | `claw-agent::AgentSystem` | [Rust API](rust-api.md) |
| ESP-IDF C 应用 | `claw_agent.h` | [C API](c-api.md) |
| 只使用 LLM client | `claw-api` | `crates/claw-api/README.md` |
| 实现新平台 backend | `claw-interface` | `crates/claw-interface/README.md` |
| 只使用 Memory/Skill/Sandbox 等 | 对应同名 crate | crate README + rustdoc |

## 生成 Rust API 文档

```bash
cargo doc --workspace --no-deps
```

生成结果位于 `target/doc/`。本地打开：

```bash
cargo doc --workspace --no-deps --open
```

## API 稳定性

15 个公开 crates 使用 `cargo-public-api` snapshot。`check.sh` 会重新生成临时 API surface 并与 `snapshots/*.txt` 比较。

Snapshot 只守护“哪些符号公开”，不替代以下契约：

- 生命周期顺序。
- ownership 与释放责任。
- blocking/non-blocking 行为。
- thread/async/ISR 约束。
- 错误语义。
- feature 导致的 ABI shape。
- 持久化 schema 与恢复行为。

这些内容由本目录、rustdoc、C header 和测试共同维护。

## 下层 API 边界

| Crate | 公开抽象 | 不应负责 |
| --- | --- | --- |
| `claw-api` | LLM request/response、SSE、retry、media | Session、Tool policy、网络实现 |
| `claw-context` | Block placement、render、version | 拉取 Memory/Skill |
| `claw-memory` | Transcript/Profile/Long-term stores | LLM transport、Session actor |
| `claw-tool` | Tool registry/set/runner | Agent graph、用户交互 UI |
| `claw-permission` | Action -> decision | 执行 Tool 或显示对话框 |
| `claw-persistence` | Typed durable entries | 定义各 domain state 的意义 |
| `claw-interface` | 平台依赖 traits | ESP-IDF 具体调用 |
| `claw-sys` | ESP-IDF 实现 | Agent domain policy |

## 错误处理约定

- Rust library 返回具体 error enum，并通过 `#[source]`/`#[from]` 保留底层错误。
- `claw-agent` 以上的 CLI/应用边界可以将错误格式化为文本。
- C ABI 把错误映射为 `esp_err_t`，异步 Runtime error 还可能通过 Session event stream 返回。
- 调用方不能只检查输出 JSON 或日志而忽略函数返回值。

## 相关文档

- [并发模型](../architecture/concurrency-model.md)
- [Memory 与 ownership](../architecture/memory-model.md)
- [安全模型](../architecture/security-model.md)
- [Examples](../examples.md)
