# Examples

所有命令默认从 `agent-framework/` 运行。大部分 Example 使用 `MemFs` 或 scripted backend，不需要网络或 API key。

## 推荐学习顺序

| 顺序 | Example | 学到什么 |
| ---: | --- | --- |
| 1 | `claw-context/build_context` | Context blocks、排序、reminder 与 version |
| 2 | `claw-permission/approval_flow` | Allow/Ask/Deny 与 grant |
| 3 | `claw-tool` 相关逻辑通过 Agent Example | Tool 注册、调用和结果流 |
| 4 | `claw-agent/tool_submit_loop` | 完整 AgentSystem + Session stream |
| 5 | `claw-sandbox/sandbox_demo` | 虚拟根、只读系统区和路径逃逸拒绝 |
| 6 | `claw-persistence/basic` | Typed state、dirty generation 与恢复 |

## Agent

### `tool_submit_loop`

```bash
cargo run -p claw-agent --example tool_submit_loop \
  --target x86_64-unknown-linux-gnu
```

离线构造完整 AgentSystem、注册 Tool、提交消息并读取 Turn events。预期看到 `time_now` 注册和一条模拟回复。

## LLM API

| Example | 命令 | 说明 |
| --- | --- | --- |
| `chat` | `cargo run -p claw-api --example chat` | Blocking chat 与结构化 JSON |
| `async_client` | `cargo run -p claw-api --example async_client` | Async/streaming client |
| `config_and_retry` | `cargo run -p claw-api --example config_and_retry` | Config validation 与 retry policy |
| `errors` | `cargo run -p claw-api --example errors` | Typed error 分类和 retryable 判断 |
| `media` | `cargo run -p claw-api --example media` | Inline/local/remote media 形态 |

这些 Example 默认使用 stub transport；如源码注释另有说明，以 Example 自身前置条件为准。

## Context 与依赖注入

```bash
cargo run -p claw-context --example build_context
cargo run -p claw-interface --example di_seams
```

`di_seams` 展示同一业务逻辑怎样在不同 FS/HTTP/Timer backend 上运行。

## Memory 与 Persistence

```bash
cargo run -p claw-memory --example conversation
cargo run -p claw-persistence --example basic
```

`conversation` 展示 Turn 级 Transcript；`basic` 展示 singleton/collection 的 typed state 与 reload。

## Permission

```bash
cargo run -p claw-permission --example approval_flow
cargo run -p claw-permission --example custom_policy
cargo run -p claw-permission --example policy_chain
```

- `approval_flow`：Ask 后保存 grant，重试不重复询问。
- `custom_policy`：自定义 PermissionPolicy。
- `policy_chain`：多个 policy 的组合顺序。

## Sandbox

```bash
cargo run -p claw-sandbox --example sandbox_demo
```

预期看到 `/sandbox` 与 `/shared/data` 写入成功，`/system` 写入、`/etc/passwd` 和 `..` traversal 被拒绝。

## Skill

```bash
cargo run -p claw-skill --example catalog
cargo run -p claw-skill --example load_context
cargo run -p claw-skill --example multi_root
```

- `catalog`：扫描 Skill 并生成 JSON/prompt catalog。
- `load_context`：激活一个 Skill document。
- `multi_root`：DATA/SYSTEM 多根合并和同 ID 优先级。

## Logging

```bash
cargo run -p claw-log --example fmt_demo
```

用于查看 plain log 与 structured trace 的格式差别。

## Host CLI

Host CLI 不是 Example target，但它是最完整的真实模型演示：

```bash
cp crates/claw-cli/.env.example crates/claw-cli/.env.local
cargo run -p claw-cli --bin claw-agent-chat
```

配置见 [Configuration](configuration.md)。

## Profiling

```bash
cargo run --profile profiling -p claw-agent-profile -- agent-init
cargo run --profile profiling -p claw-agent-profile --bin claw-agent-stack
```

第一个命令生成 DHAT 报告，第二个在 bounded worker stack 下执行 init smoke test。详见 [`bench/README.md`](../bench/README.md)。

## ESP-IDF 设备测试应用

`claw-sys` 与 `claw-api` 各有独立 ESP-IDF test app：

```bash
cd crates/claw-sys/tests/esp_idf
cp main/test_secrets.h.example main/test_secrets.h
./run.sh build
```

```bash
cd crates/claw-api/tests/esp_idf
cp main/test_secrets.h.example main/test_secrets.h
./run.sh build
```

填入的 Wi-Fi/API secrets 不得提交。`run.sh` 支持 `build`、`flash`、`monitor` 和默认的完整流程。
