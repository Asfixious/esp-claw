# Debugging

调试目标是先判断问题属于 domain、platform adapter、C ABI、provider protocol 还是产品路由，再打开对应级别的观测能力。

## 最小复现顺序

1. 用单 crate unit/integration test 复现。
2. 用 `MemFs`/HTTP mock/ImmediateTimer 隔离平台。
3. 用 Host CLI + real HTTP 复现 provider 问题。
4. 用 `claw-sys`/`claw-api` device test app 复现 ESP-IDF adapter。
5. 最后进入完整 Edge Agent + Event Router + IM 链路。

越早在 Host 复现，迭代越快。

## Host CLI logs

运行：

```bash
cargo run -p claw-cli --bin claw-agent-chat
```

输出文件：

```text
crates/claw-cli/simulator_log.log
crates/claw-cli/simulator_trace.log
```

Plain log 适合第一处 error；trace 适合 Session/Turn/Agent/Iteration/Tool 的时间关系。

## 转换 Chrome trace

```bash
uv sync --all-packages --all-groups
uv run --package claw-trace \
  claw-trace-chrome crates/claw-cli/simulator_trace.log \
  -o trace.json
```

在 Perfetto UI 打开 `trace.json`。重点查看：

- 同一 Session 的 Turn 是否正确嵌套。
- LLM stream 是否在 header/body read 停滞。
- Tool call 是否进入 detached path。
- Background completion 是否唤醒正确 root。
- Context/persistence boundary 是否异常拉长。

Trace exporter 的 logical process/thread mapping 见 `crates/claw-log/scripts/README.md`。

## 日志模式

### Production

`prod_logging` 保留 lightweight `log`，编译掉 structured tracing。适合 size 和普通串口排障。

### Rich

```bash
export CLAW_DEBUG=1
./build-idf-staticlib.sh
```

`rich_logging` 启用 structured tracing 和 cache usage。它改变 binary size，并包含 `cache_profile` ABI；C consumer 必须匹配。

`RUST_LOG` 不生效。`claw-log` 的运行时过滤级别由初始化 API 参数决定，编译期 ceiling 由 Cargo feature 决定。

## Context observer

`intrusive-observability` feature 提供 Context snapshot observer，用于分析实际送给模型的 system/history/reminders。它是侵入式诊断能力，不应默认进入生产固件。

Host viewer：

```bash
uv run --script crates/claw-context/scripts/context_viewer.py
```

## LLM tape

当问题与 provider chunk timing/SSE 边界有关时，使用 recorder 捕获原始 response bytes 和时间：

```bash
uv run --package llm-tape llm-tape record \
  --listen 0.0.0.0:8787 \
  --upstream https://api.example.com \
  --output bench/tapes/run.jsonl
```

Replay：

```bash
uv run --package llm-tape llm-tape replay \
  --listen 0.0.0.0:8787 \
  bench/tapes/run.jsonl
```

Tape 保存 provider response 原文，可能包含私有 context。它不保存 auth header/request body，但仍要按敏感数据处理。

## Memory profiling

```bash
cargo run --profile profiling -p claw-agent-profile -- agent-init
```

DHAT 输出默认在：

```text
target/profiles/agent-init.dhat.json
```

检查 binary size：

```bash
cargo build --profile profiling -p claw-agent-profile
size target/profiling/claw-agent-profile
size -A target/profiling/claw-agent-profile
```

Bounded stack smoke：

```bash
cargo run --profile profiling -p claw-agent-profile --bin claw-agent-stack
```

Host stack estimate 不是设备 high-water mark 的替代品。

## Device tests

使用最小 test app 区分 adapter 与产品 wiring：

```bash
cd crates/claw-sys/tests/esp_idf
./run.sh build
```

```bash
cd crates/claw-api/tests/esp_idf
./run.sh build
```

需要 device/Wi-Fi/provider 的 tests 使用本地 `test_secrets.h`，禁止提交。

## C ABI 调试

遇到 crash/UAF/leak 时检查：

- Header 与 linked archive 是否同一 commit/features。
- Union member 是否与 event kind 匹配。
- 每个成功 receive 是否 exactly once free。
- C string 是否有效 UTF-8 且在调用期间存活。
- Lifecycle 是否被多个 task 并发管理。
- 同一 Session 是否有多个 stream consumer。

## Cooperative starvation

症状包括 SSE 长时间无进展、其他 Session 饿死、watchdog 或 event pump timeout。定位一次 poll 中的同步工作：

- 大 JSON parse/render。
- blocking filesystem/persistence。
- C Capability 长调用。
- 未 drain 的 HTTP body。
- 无 yield 的批循环。

不要用增大 timeout 掩盖没有 poll/yield 的路径。

## 报告问题

提供脱敏后的 trace/log 时说明时间轴、Session ID、Turn origin、最后一个正常 event 和预期 terminal boundary。不要只说“卡住了”。
