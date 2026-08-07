# Development Guide

本指南描述日常开发循环。第一次配置环境请先阅读[安装指南](../installation.md)。

## 环境基线

| 工具 | 用途 | CI 基线 |
| --- | --- | --- |
| Rust stable | Host check/test/doc | 1.96.0 |
| rustfmt + Clippy | 格式和 lint | stable components |
| cargo-public-api | 公开 API snapshots | stable latest locked install |
| Python + uv | Trace、serial log、LLM tape | Python 3.11+ |
| Espressif Rust | 设备 Rust archive | toolchain 1.95.0.0 |
| ESP-IDF | 固件和 device tests | v6.1.x |
| zstd | archive 打包/解压 | 系统工具 |

## 第一次 checkout

```bash
cd agent-framework
rustup toolchain install stable
rustup component add rustfmt clippy
cargo +stable install cargo-public-api --locked
cargo check --workspace --all-targets
```

需要 Python 工具时：

```bash
uv sync --all-packages --all-groups
```

## 日常开发循环

1. 找到责任 crate 和当前行为测试。
2. 先写或收紧失败复现。
3. 实现最小 coherent diff。
4. 运行 crate 局部测试和 Clippy。
5. 运行 workspace quality gates。
6. 检查 public API、文档、设备 archive 与固件影响。

局部命令：

```bash
cargo fmt --all --check
cargo test -p claw-core
cargo clippy -p claw-core --all-targets --all-features -- -D warnings
```

提交前：

```bash
./check.sh
./harness.sh
```

## 如何定位修改点

从责任而不是文件名出发：

- LLM wire format/SSE/retry：`claw-api`。
- Session/Turn/Iteration/multiagent：`claw-core`。
- 应用装配和公开入口：`claw-agent`。
- Tool lifecycle/visibility/execution：`claw-tool`。
- Tool 风险决策：`claw-permission`。
- Context placement/render：`claw-context`。
- Transcript/Profile/facts：`claw-memory`。
- Durable state mechanism：`claw-persistence`。
- Platform interface：`claw-interface`。
- ESP-IDF concrete implementation：`claw-sys`。
- Pointer/union/C ownership：`claw-cabi`。

完整边界见[代码结构](code-structure.md)。

## 错误设计

Production crates 必须保留 typed error chain：

```rust
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("failed to load profile")]
    Load {
        #[source]
        source: FsError,
    },
}
```

原则：

- 一个 variant 必须能被该函数实际返回。
- 同一底层 error 出现在不同阶段时，用 stage-specific variants 保留上下文。
- 不用 `to_string()` 传播错误。
- CLI、日志、FFI 等边界才把 typed error 渲染为文本或 `esp_err_t`。
- 缺失必需配置立即报错，不猜 endpoint 或 secret。

## Async 与并发

- 不在 `.await` 期间持有 `Mutex`/`RwLock` guard。
- 将需要的数据 clone/move 出临界区后再 await。
- 无界 channel 只有在 domain 明确接受且有上游约束时使用；外部输入必须有 backpressure/resource limit。
- CPU 或长同步工作不能直接放在设备 cooperative poll 中。
- Cancel/interrupt path 必须能关闭 content boundaries 并回收 Tool/Agent ownership。
- Test double 用尽 scripted response 时应失败，不返回默认假成功。

## Public API

新增公开 API 前先问：

1. 这是产品需要还是测试便利？
2. 应该属于上层 facade 还是下层 domain？
3. 类型是否完整表达行为，还是仍依赖 side parameter？
4. ownership、errors、threading 和 examples 是否能写清？

有意变更后：

```bash
./update-public-api-snapshots.sh
git diff -- snapshots/
```

## 资源与固件约束

- 避免大栈对象；设备 worker stack 有明确上限。
- Collection 已知容量时预分配，循环中复用 buffer。
- Tool schema、prompt 和 trace 可能显著增加 flash/RAM。
- 对性能路径先测量，再优化；使用 `bench/` workload 和 binary `size`。
- Release 使用 `panic=abort`，不能依赖 unwinding cleanup。

## 文档工作流

修改行为时同步：

- crate rustdoc/README。
- `docs/api` 的生命周期和边界。
- `docs/development` 的命令/工具链。
- `docs/architecture` 的稳定原则。
- 重大选择新增 ADR。

若影响普通 ESP-Claw 用户，再同步根文档站中英文页面并运行对应文档检查。

## 提交修改

见 [`CONTRIBUTING.md`](../../CONTRIBUTING.md)。提交说明应包含风险、测试、API/ABI、持久化和固件影响，而不只是列出文件。
