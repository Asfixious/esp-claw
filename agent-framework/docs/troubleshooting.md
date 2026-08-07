# Troubleshooting

先从第一处错误开始排查。Cargo、CMake 和 linker 后续往往会产生大量级联错误，只复制最后一行通常没有帮助。

## `cargo-public-api is required`

安装工具：

```bash
cargo +stable install cargo-public-api --locked
```

确认当前 shell 可找到：

```bash
cargo public-api --version
```

## `Prebuilt Rust archive not found`

原因通常是：

- `IDF_TARGET` 不在 `esp32s3/esp32c5/esp32p4/esp32s31`。
- archive 被删除。
- 自定义 `CLAW_RUST_PREBUILT_DIR` 布局错误。

期望路径：

```text
<prebuilt-root>/<IDF_TARGET>/libclaw_cabi.a.zst
```

如果目标受支持且你修改了 Rust，运行 `./build-idf-staticlib.sh`。不要复制另一个芯片的 archive 冒充当前目标。

## `zstd is required`

普通固件 build 也需要 `zstd`，因为 committed archive 是压缩文件。安装 `zstd` 并确认：

```bash
zstd --version
```

## ESP-IDF version/ABI fingerprint 失败

Framework 只支持 ESP-IDF v6.1.x，并校验 `esp_http_client_config_t` layout。不要禁用 guard。

```bash
idf.py --version
git -C "$IDF_PATH" rev-parse HEAD
```

切换到已验证的 `release/v6.1`，清理对应 application build 目录后重新配置。不要用旧 archive 配新 header。

## `cargo +esp` 不存在

安装并加载 Espressif toolchain：

```bash
cargo install espup --version 0.17.1 --locked
espup install
. "$HOME/export-esp.sh"
```

随后再导出 ESP-IDF。

## Archive 校验失败

`./build-idf-staticlib.sh --check` 会在临时目录重建，并逐个比较 committed archive。失败说明源码、toolchain/profile/feature 或 archive 不一致。

处理方式：

1. 确认使用仓库要求的 toolchain 和 IDF。
2. 运行 `./build-idf-staticlib.sh` 刷新全部目标。
3. 再运行 `--check`。
4. 审查四个 binary diff 都是预期结果。

不要只更新当前手边的一块板。

## Host CLI 报缺少 `CLAW_LLM_*`

```bash
cp crates/claw-cli/.env.example crates/claw-cli/.env.local
```

三个值都必须非空。当前 CLI base URL 通常需要包含 provider 要求的 API prefix，例如 OpenAI-compatible endpoint 的 `/v1`。

## CLI 能启动但模型请求失败

依次检查：

- endpoint 与 backend 是否兼容；当前 CLI 固定 OpenAI-compatible。
- model ID 是否存在。
- API key 是否有权限。
- base URL 是否包含正确 path prefix。
- 系统时间、DNS、代理和 TLS certificate。
- `simulator_log.log` 中的第一处 transport/provider error。

不要把完整 key、Authorization header 或私有 prompt 发到 issue。

## Session submit 返回 invalid state/not found

- Runtime 必须已经 `start_all`/`claw_agent_start`。
- Session 必须先 create，再 open。
- C ABI 的 submit/receive 只能操作已打开 Session。
- `respond` 必须使用最新 `InputRequested` 的 request ID。
- close 后 Session identity 仍存在，但需要重新 open；delete 后不可恢复。

## 收到 `TurnEnded` 后不再读取，漏掉后台结果

`TurnEnded` 只关闭一个 Turn。长期 Session stream 可能随后收到 detached completion 唤醒的新 Turn。只有 `Closed` 是 terminal event。

## C 侧内存泄漏或 double free

每次 `claw_agent_session_receive` 返回 `ESP_OK` 后，event 都必须 exactly once 调用：

```c
claw_agent_event_free(&event);
```

即使忽略某个 delta/boundary event 也要 free。只能读取 `event.kind` 对应的 union member；不要保存内部字符串指针到 free 之后。

## Event Router 没有输出

检查：

- `cap_agent` 是否注册并启动。
- Session 是否有 event pump，且没有第二个 consumer 抢读。
- Router rule 是否匹配 `source_cap=agent,event_type=out_message`。
- channel/chat route 是否随输入传入。
- IM Gateway 是否有对应 channel backend。
- output 是否超过 pump buffer 并产生 error event。

产品链路详见 `components/claw_capabilities/cap_agent/EVENT_ROUTER.md`。

## Stream 卡住或设备 watchdog

设备 Runtime 是 cooperative async。一次 poll 内的同步 filesystem、JSON、context、Tool 或 C callback 工作过长，会阻塞同一 OS thread。

建议：

- 使用 bounded batch，并在长循环间 yield。
- 不在 `.await` 前后持有 mutex guard。
- 不把 CPU 密集工作放在 async poll。
- 检查 HTTP body 是否持续 drain。
- 开 rich logging/trace 确认最后一个有进展的 span。

## Clippy 只在 all-features 失败

单 crate 默认 feature 通过不代表支持的 feature matrix 通过。复现：

```bash
cargo clippy -p <crate> --all-targets --all-features -- -D warnings
```

`claw-cabi` 的两个 logging feature 互斥，按 `harness.sh` 的独立组合检查，不要直接对它使用无意义的 `--all-features` 形态。

## Public API snapshot diff

先判断 diff 是否真是产品 API：

- 意外 `pub`：收窄为 `pub(crate)`/`pub(super)`。
- 合理新 API：补 rustdoc、测试、API 文档和迁移说明，再更新 snapshot。
- 只为测试暴露：改用内部 test helper，不更新 public API。

## Trace 怎么看

```bash
uv sync --all-packages --all-groups
uv run --package claw-trace \
  claw-trace-chrome <path-to-log> -o trace.json
```

将 `trace.json` 导入 Perfetto UI。更完整的排查流程见[调试指南](development/debugging.md)。

## 仍无法解决

提交问题时附上：版本、target、features、完整命令、第一处错误、最小复现、是否 prebuilt/source build，以及脱敏后的 log/trace。说明问题能否在 Host test double 下复现，这能快速区分 domain bug 与 ESP-IDF adapter bug。
