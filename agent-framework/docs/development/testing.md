# Testing

测试按“纯 domain -> Host integration -> device adapter -> 完整固件”分层。每个改动应选择能覆盖风险的最小层，再运行合并门禁。

## 测试金字塔

| 层 | 目标 | 典型位置 |
| --- | --- | --- |
| Unit | 单个 parser/state/policy/invariant | `src/**/tests`、`#[cfg(test)]` |
| Crate integration | 公开 API 与多个内部模块 | `crates/<name>/tests/` |
| Agent matrix | Session、Tool、failure、persistence、multiagent | `crates/claw-agent/tests/` |
| Host end-to-end | CLI/real HTTP/scripted provider | examples、ignored live tests |
| Device selftest | ESP-IDF HTTP/Timer/FFI | `claw-sys`/`claw-api` test apps |
| Firmware build | CMake、archive、board dependency | `application/edge_agent` CI |

## 快速局部测试

```bash
cargo test -p claw-tool
cargo test -p claw-agent --test session_api
cargo clippy -p claw-core --all-targets --all-features -- -D warnings
```

测试名应描述行为和条件，不使用含糊的 `test_1`。

## `check.sh`

```bash
./check.sh
```

执行：

1. `cargo check --workspace --all-targets`
2. 对 15 个 public crates 生成临时 `cargo public-api` surface
3. 与 `snapshots/*.txt` 做 diff

它不运行完整 tests，也不替代 harness。

## `harness.sh`

```bash
./harness.sh
```

执行：

- `cargo fmt --all --check`
- workspace Clippy `-D warnings`
- 每个 feature-bearing crate 的 all-features Clippy/check/test
- `claw-cabi` 的空、rich+cache、prod+cache 组合
- `claw-log` 的每个 log/trace compile-time ceiling
- workspace check/test

`claw-cabi` 的 logging features 互斥，因此 harness 不对它盲目使用 `--all-features`。

## Public API snapshots

有意变更：

```bash
./update-public-api-snapshots.sh
git diff -- snapshots/
```

审查每一行：新增 public item、删除/改名、generic bound、error variant 与 feature shape 都可能影响调用方。

不应更新 snapshot 的情况：

- 意外 `pub`。
- 只为测试方便的 constructor/knob。
- 本应隐藏的 implementation module。
- 未完成或尚无产品契约的 API。

## Golden/fixture tests

Agent matrix 使用 CSV fixtures 覆盖大量组合。Skill golden 只在显式设置更新环境时刷新：

```bash
CLAW_UPDATE_GOLDEN=1 cargo test -p claw-skill --test golden
```

刷新后必须人工审查 expected document，防止 parser regression 被自动接受。

## Live network tests

真实 provider/network tests 默认应 ignored 或使用明确 mock endpoint，避免普通 CI 依赖 secrets 和公网。运行 live test 前确认不会把 request/response 写进提交日志。

## Device selftests

### `claw-sys`

```bash
cd crates/claw-sys/tests/esp_idf
cp main/test_secrets.h.example main/test_secrets.h
./run.sh build
```

覆盖 ESP-IDF HTTP、streaming、Timer/Thread/FFI 适配。

### `claw-api`

```bash
cd crates/claw-api/tests/esp_idf
cp main/test_secrets.h.example main/test_secrets.h
./run.sh build
```

覆盖 provider request、SSE 与 device HTTP integration。

两个脚本还支持 `flash`、`monitor` 和完整默认流程。

## Archive reproducibility

Rust device closure 或 embedded resources 变化后：

```bash
./build-idf-staticlib.sh
./build-idf-staticlib.sh --check
```

`--check` 必须在干净/预期 toolchain 下通过。四个目标都要一致。

## Firmware build

至少构建一个受影响目标：

```bash
cd ../application/edge_agent
idf.py bmgr -c ./boards -b esp32_S3_DevKitC_1
idf.py build
```

涉及 target resolver、libc、HTTP ABI 或 C header 时，应覆盖所有相关芯片家族，而不是只构建 S3。

## 文档测试

Framework Markdown 至少检查链接和命令。修改根文档站时必须运行：

```bash
cd ../docs
pnpm run check:doc-lines
pnpm run build
```

Rustdoc：

```bash
cargo test --doc --workspace
cargo doc --workspace --no-deps
```

## 按改动类型选择验证

| 改动 | 最低验证 |
| --- | --- |
| 纯内部 Rust refactor | crate tests + check + harness |
| Public Rust API | 上述 + snapshot + rustdoc/example |
| C ABI | 上述 + archive + device/firmware build |
| ESP-IDF HTTP/Timer | device selftest + 多 target build |
| Persistence schema/recovery | failure/recovery matrix + restart tests |
| Session/multiagent/concurrency | stress/matrix + trace ordering tests |
| Tool/permission | public API + allow/ask/deny/cancel/error tests |
| Prompt/schema/resource | Agent matrix + archive refresh/size review |
| 根文档站 | bilingual line check + site build |

## 测试失败原则

- Scripted response 用尽应失败，不能返回默认值。
- Error path 也要断言 cleanup/ownership，而不只看 error string。
- Async test 必须有明确 timeout，避免死锁无限挂起。
- 不通过增加 sleep 修复 race；使用事件、channel 或 deterministic scheduler boundary。
- 不降低 lint/测试强度来让变更通过。
