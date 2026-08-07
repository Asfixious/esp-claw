# Installation

Framework 有三种不同环境。先确定自己的目标，避免为了普通固件构建安装不必要的工具。

| 使用场景 | Rust stable | Espressif Rust | ESP-IDF | zstd |
| --- | --- | --- | --- | --- |
| 运行 Host 示例和测试 | 必需 | 不需要 | 不需要 | 不需要 |
| 使用 committed archive 构建固件 | 不需要 | 不需要 | v6.1.x | 必需 |
| 修改 Rust 并重建设备 archive | 必需 | 必需 | v6.1.x | 必需 |

CI 以 Linux 为权威环境。其他 OS 可以运行 Host Rust，但最终设备兼容性应以 ESP-IDF 支持矩阵和 CI 为准。

## Host Rust 开发

安装 [rustup](https://rustup.rs/) 后：

```bash
rustup toolchain install stable
rustup component add rustfmt clippy
```

当前 CI 使用 Rust 1.96.0；`rust-toolchain.toml` 跟随 stable。如果 stable 升级导致行为变化，应先在 CI 镜像和 snapshots 中显式验证。

验证：

```bash
cd agent-framework
cargo check --workspace --all-targets
cargo test --workspace --all-targets
```

### Public API 工具

`check.sh` 需要：

```bash
cargo +stable install cargo-public-api --locked
```

### Python 工具

Trace exporter、serial log 和 LLM tape 使用 Python 3.11+ 与 uv：

```bash
uv sync --all-packages --all-groups
```

这些工具共享根 `uv.lock` 和 `.venv`。运行时始终显式指定 package，例如：

```bash
uv run --package claw-trace claw-trace-chrome --help
```

## 使用预编译 archive 构建固件

安装 ESP-IDF v6.1.x。当前验证版本是 `release/v6.1` @ `3e24f276fa9be527762c0c0891ad3fbb00029f06`。

导出 IDF：

```bash
. "$IDF_PATH/export.sh"
```

安装额外依赖：

```bash
python -m pip install esp-bmgr-assist
```

系统还必须提供 `zstd`。常见 Linux 安装方式：

```bash
sudo apt-get install zstd
```

然后构建：

```bash
cd application/edge_agent
idf.py bmgr -c ./boards -b esp32_S3_DevKitC_1
idf.py build
```

固件 CMake 会根据 `IDF_TARGET` 选择相同名字的 prebuilt 目录。

## 重建设备 Rust archive

除了上述依赖，还需要 Espressif Rust toolchain。与 CI 对齐的安装方式是：

```bash
cargo install espup --version 0.17.1 --locked
espup install \
  --std \
  --stable-version 1.96.0 \
  --toolchain-version 1.95.0.0 \
  --targets esp32s3,esp32c5,esp32p4
```

加载 `espup` 生成的环境，再加载 ESP-IDF：

```bash
. "$HOME/export-esp.sh"
. "$IDF_PATH/export.sh"
```

`build-idf-staticlib.sh` 也会尝试加载这两个环境，但显式加载便于提前发现配置错误。

重建四个目标：

```bash
cd agent-framework
./build-idf-staticlib.sh
```

生成位置：

```text
crates/claw-cabi/prebuilt/esp32s3/libclaw_cabi.a.zst
crates/claw-cabi/prebuilt/esp32c5/libclaw_cabi.a.zst
crates/claw-cabi/prebuilt/esp32p4/libclaw_cabi.a.zst
crates/claw-cabi/prebuilt/esp32s31/libclaw_cabi.a.zst
```

校验 committed artifacts：

```bash
./build-idf-staticlib.sh --check
```

## 可选工具

- Perfetto UI：查看 Chrome trace。
- `size`/`size -A`：检查 host profiling binary sections。
- `pkg-config`：CI 的 Host/设备检查依赖。
- Node.js/pnpm：只在修改仓库根文档站时需要。

## 卸载与清理

Cargo build artifacts 位于 `target/`。如确需释放空间，可运行 `cargo clean`，但这会删除整个 workspace 的增量缓存。不要删除或手工编辑 `crates/claw-cabi/prebuilt/` 中的 committed archives。
