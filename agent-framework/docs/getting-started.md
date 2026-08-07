# Getting Started

本教程在 10 分钟内完成三个目标：运行一个完全离线的 Agent、看懂 Session event stream，并知道怎样切换到真实模型或 ESP-IDF 固件。

## 前置条件

- Linux、macOS 或 Windows 上可用的 Rust stable。
- Git checkout 已包含 `agent-framework/`。
- 本教程的离线部分不需要 ESP-IDF、网络或 API key。

从 `agent-framework/` 目录开始：

```bash
rustup toolchain install stable
rustup component add rustfmt clippy
cargo --version
```

如果环境尚未准备好，先阅读[安装指南](installation.md)。

## 第一个 Agent

运行 hermetic example：

```bash
cargo run -p claw-agent --example tool_submit_loop \
  --target x86_64-unknown-linux-gnu
```

这个 Example 会：

1. 创建内存文件系统和 scripted HTTP backend。
2. 注册一个 `time_now` Tool。
3. 构造 `AgentSystem` 并绑定模拟 LLM。
4. 创建 persistent Session。
5. 提交用户消息并消费 event stream。
6. 在 `TurnEnded` 处结束本次读取。

预期输出包含：

```text
registered tool `time_now`
session `session-1` events:
  > Hello from the agent ...
```

没有网络请求是正常行为。LLM response 已写在 Example 的 scripted backend 中，目的在于先验证 Runtime 控制流。

源码位于 [`crates/claw-agent/examples/tool_submit_loop.rs`](../crates/claw-agent/examples/tool_submit_loop.rs)。建议依次找到：

- `AgentSystem::with_tool_groups`
- `link_api`
- `start_all`
- `new_session`
- `open_session`
- `SessionControl::append`
- `SessionEvent::Turn`

这组 API 构成最小 Agent 生命周期。

## 第二个 Example：Context

```bash
cargo run -p claw-context --example build_context \
  --target x86_64-unknown-linux-gnu
```

你会看到 system prefix、ephemeral reminders 和 context version。这个 Example 解释为什么 Context 由有顺序的 blocks 组成，以及相同内容为什么不会让 version 增长。

## 运行测试

先跑 workspace tests：

```bash
cargo test --workspace --all-targets
```

贡献者还应安装 `cargo-public-api` 并运行完整入口：

```bash
cargo +stable install cargo-public-api --locked
./check.sh
./harness.sh
```

`harness.sh` 会运行 feature matrix，因此明显慢于单个 `cargo test`。

## 连接真实模型

复制 CLI 配置：

```bash
cp crates/claw-cli/.env.example crates/claw-cli/.env.local
```

填写：

```dotenv
CLAW_LLM_API_KEY=your-key
CLAW_LLM_BASE_URL=https://api.openai.com/v1
CLAW_LLM_MODEL=your-model
```

运行：

```bash
cargo run -p claw-cli --bin claw-agent-chat
```

输入一条普通消息会自动创建 persistent Session。输入 `/` 可查看补全；常用命令见[用户指南](user-guide.md)。CLI 的 memory、log 和 trace 都写在 `crates/claw-cli/` 下，不会进入设备存储。

## 构建 ESP-Claw 固件

如果只使用仓库提交的 Rust archive，不需要 Espressif Rust toolchain。安装并导出 ESP-IDF v6.1 后：

```bash
cd ../application/edge_agent
idf.py bmgr -c ./boards -b esp32_S3_DevKitC_1
idf.py build
```

构建过程会从 `crates/claw-cabi/prebuilt/esp32s3/` 解压并链接 archive。修改 Rust 代码时才需要重新生成 archive；见[构建系统](development/build-system.md)。

## 下一步

- 想嵌入 Rust：阅读 [Rust API](api/rust-api.md)。
- 想从 C 调用：阅读 [C API](api/c-api.md)。
- 想看更多组件示例：阅读 [Examples](examples.md)。
- 想理解 Session 与 Tool 数据流：阅读[架构总览](architecture/overview.md)。
- 命令失败：先查 [Troubleshooting](troubleshooting.md)。
