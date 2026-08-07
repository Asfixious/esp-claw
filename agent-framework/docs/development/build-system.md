# Build System

Framework 同时参与 Cargo workspace 和 ESP-IDF CMake build。理解两条构建链的交点，是避免“Host 能过、固件不能链接”问题的关键。

## Host Cargo build

根 `Cargo.toml` 包含：

- `crates/*`：生产 crates。
- `c-legacy/*`：过渡 adapter。
- `bench/*`：Host profiling crates。
- 两个 device-only selftest staticlib crate。

默认 members 是 `claw-agent` 与 `claw-interface`，但 CI 始终使用 `--workspace --all-targets`。

```bash
cargo check --workspace --all-targets
cargo test --workspace --all-targets
```

Workspace 默认 target 未设置，普通 Cargo 命令走 Host。设备 target 由 build script/CMake 显式传入。

## Release profile

```toml
[profile.release]
panic = "abort"
opt-level = "z"
codegen-units = 1
lto = false
```

- `panic=abort`：ESP-IDF targets 不 unwind。
- `opt-level=z`：优先固件 size。
- 单 codegen unit：提高跨模块优化机会。
- LTO 关闭：避免 final GCC linker 无法消费 LLVM bitcode archive。

`profiling` 继承 release，但保留 debug location 并不 strip。

## ESP-IDF component

根 `CMakeLists.txt` 注册：

- `claw-sys/csrc` 的 C compatibility/log shims。
- `claw-cabi/include` 的公开 header。
- ESP HTTP、Timer、TLS、mbedtls、FreeRTOS、pthread dependencies。
- 一个 imported static library `claw_cabi_rust`。

最终 component 使用 `WHOLE_ARCHIVE`，确保 Rust staticlib 所需符号不因普通 archive dead stripping 被漏掉。

## Prebuilt 路径

默认 `CLAW_RUST_PREBUILT_DIR` 非空：

```text
CMake
  -> resolve IDF target
  -> find <root>/<target>/libclaw_cabi.a.zst
  -> zstd decompress into build directory
  -> import libclaw_cabi.a
  -> final ESP-IDF link
```

优点是固件使用者不需要 Rust toolchain。缺点是每次 Rust/ABI/toolchain 变化都要刷新 artifacts。

## Source 路径

把 prebuilt root 设为空：

```bash
idf.py -DCLAW_RUST_PREBUILT_DIR= build
```

CMake 会运行类似：

```bash
cargo +esp build --release -p claw-cabi \
  --features prod_logging \
  --target <resolved-target> \
  -Z build-std=std,panic_abort
```

`CLAW_DEBUG=1` 时选择 `rich_logging`；production 模式还启用 `optimize_for_size` build-std feature。

## Target 解析

`cmake/ClawRustTarget.cmake` 将：

- `esp32s3`
- `esp32c5`
- `esp32p4`
- `esp32s31`

映射为对应 Rust ESP-IDF target，并解析 IDF v6.1 libc archive target。新增 target 必须同时更新 resolver、tests、espup install targets、archive builder、prebuilt layout 和 CI firmware matrix。

## Archive builder

```bash
./build-idf-staticlib.sh
```

脚本会：

1. 加载 espup 与 ESP-IDF 环境。
2. 校验 IDF checkout/version。
3. 为四个 targets 运行 release `claw-cabi` build。
4. 用 zstd 压缩 archive。
5. 写入 committed prebuilt 目录。

校验模式：

```bash
./build-idf-staticlib.sh --check
```

它在临时目录重建，不覆盖仓库 artifacts，并逐字节比较每个目标。

## Build scripts 与 embedded resources

`claw-core` 使用 `manifest_gen/main.rs` 作为 Cargo build script。它在编译期解析 `resources/agents/<kind>/` 下的 manifest、instructions 和 Tool metadata，生成 typed catalog。非法 JSON、缺失文件或不一致 metadata 应在 build 期失败。

Tool schema/usage、prompt 与 Agent instructions 通过 `include_str!`/generated code 进入 binary。修改这些资源会改变固件 archive，即使没有 `.rs` diff。

## Public API snapshots

`check.sh` 先运行 Cargo check，再对 15 个 crates 调用 `cargo public-api` 并与 `snapshots/` 比较。这是 source/API build gate，不检查 C ABI binary compatibility。

## Python workspace

`pyproject.toml` 定义 uv workspace，包含 llm-tape、claw-log trace 工具和 serial log。它们共享一个 lockfile，但不参与 Rust staticlib。

## CI

`pre_check_agent_framework` 在 Rust 1.96.0 镜像中安装：

- rustfmt/Clippy
- nightly（工具需要）
- cargo-public-api
- espup/Espressif targets
- pkg-config、zstd

然后依次运行 `check.sh`、archive `--check`、`harness.sh`。固件 jobs 使用 ESP-IDF v6.1 image 解压 prebuilt archive并构建各 boards。

## 常见扩展

### 新增 crate

放入 `crates/<name>` 后会自动进入 workspace。仍需显式决定：

- 是否是 public crate，是否加入 API snapshots。
- 是否进入 `claw-agent`/`claw-cabi` dependency closure。
- 是否允许 unsafe。
- 是否需要 device feature/Host test double。
- 对 firmware size 的影响。

### 变更 C ABI

Header、Rust repr/conversion、C tests、API docs 和四个 archives 必须同一提交更新。
