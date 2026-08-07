# Configuration

Framework 配置分为五层：运行时 LLM/Session 配置、Host CLI 环境变量、Cargo features、CMake build options 和 ESP-Claw Kconfig。它们作用在不同阶段，不互相替代。

## 配置优先级

1. 调用方显式传入的 Rust/C runtime config。
2. Host CLI 的进程环境变量；`crates/claw-cli/.env.local` 只填补尚未设置的值。
3. Cargo features 决定编译进 binary 的能力和 ABI shape。
4. CMake options 决定链接 prebuilt archive 还是源码构建。
5. ESP-Claw Kconfig 决定固件编入哪些 C Capabilities、Lua modules 和 UI integrations。

Runtime 不会为缺失的必需配置猜默认 API key、model 或 endpoint。

## Host CLI 环境变量

`claw-agent-chat` 只读取三个必需变量：

| 变量 | 必需 | 示例 |
| --- | --- | --- |
| `CLAW_LLM_API_KEY` | 是 | `sk-...` |
| `CLAW_LLM_BASE_URL` | 是 | `https://api.openai.com/v1` |
| `CLAW_LLM_MODEL` | 是 | provider 支持的 model id |

将 [`crates/claw-cli/.env.example`](../crates/claw-cli/.env.example) 复制为 `.env.local`。空值会让 CLI 启动失败并明确指出缺失变量。

CLI 当前固定使用 `BackendKind::OpenAiCompatible`，timeout 为 60 秒。需要 Anthropic-compatible 或更多参数时，应通过 Rust API 构造 `ClawApiConfig`，而不是添加未被 CLI 读取的环境变量。

## Runtime LLM 配置

Rust 使用：

```rust
let config = ClawApiConfig::new(
    BackendKind::OpenAiCompatible,
    api_key,
    model,
    base_url,
);
system.link_api(config, ApiPurpose::RootAgent, true)?;
```

`ApiPurpose` 区分：

- `RootAgent`
- `Subagent`
- `Memory`
- `Compaction`

`is_default = true` 表示其他没有显式 binding 的 purpose 可以回退到该配置。替换运行中的 binding 会在下一个 turn boundary 生效，不会修改当前 in-flight turn。

C ABI 对应 `claw_agent_api_config_t` 与 `claw_agent_link_api`。初始 `claw_agent_config_t` 的四个 API 字段可以全部为空，但不能只填一部分。

## Persistence 与 Skill roots

Rust `AgentPersistenceConfig` 包含：

| 字段 | 含义 |
| --- | --- |
| `persistence_root` | Runtime state、Session、Transcript、Profile 等持久化根 |
| `skill_roots` | 按优先级扫描的 Skill 根目录 |

较早出现的 Skill root 优先。固件通常传入 DATA root 后再传 SYSTEM root，使用户安装 Skill 覆盖固件内置版本。

## Cargo features

### Runtime shape

| Crate/feature | 作用 |
| --- | --- |
| `claw-agent/multiagent` | 编译 Session-scoped subagent graph 与 tools |
| `claw-agent/intrusive-observability` | 转发 Context observer 支持 |
| `claw-agent/use_legacy_skill` | 设备端复用 C Skill registry |
| `claw-agent/cache_profile` | Provider token/cache usage diagnostics |
| `claw-core/multiagent` | Core multiagent domain |
| `claw-core/intrusive-observability` | Context snapshot observer |
| `claw-core/use_legacy_skill` | 启用过渡 C Skill adapter |

设备 `claw-cabi` 当前依赖 `claw-agent` 的 `multiagent` 与 `use_legacy_skill`。

### C ABI 与日志

| Feature | 作用 | 约束 |
| --- | --- | --- |
| `claw-cabi/prod_logging` | 轻量 `log`，编译掉 structured tracing | 普通固件默认 |
| `claw-cabi/rich_logging` | Structured tracing，编译掉重复 plain log | 自动包含 `cache_profile` |
| `claw-cabi/cache_profile` | 增加 provider usage event | 会改变 C event ABI |

`prod_logging` 与 `rich_logging` 不应同时启用。

`claw-log` 还提供 `log_max_*` 与 `trace_max_*` 编译期 ceiling。它不读取 `RUST_LOG`；运行时最大级别由初始化函数参数决定。

### Host backends 与测试

`claw-interface` 的常用 feature：

- `host-backends`：组合 diskfs、realhttp、stdthread、tokiotimer、tokioexecutor。
- `test-doubles`：组合 HTTP/Timer doubles。
- `diskfs`、`diskfs-pretty`
- `realhttp`
- `httpmock`、`timermock`
- `stdthread`、`tokioexecutor`、`tokiotimer`

其他内部 feature 包括 `claw-context/intrusive-observability`、`claw-memory/compactor-stub` 和 `claw-tool/build-support`。后两者主要用于测试或 build script，不应作为产品 feature 传播。

## CMake options

### `CLAW_RUST_PREBUILT_DIR`

默认值：`crates/claw-cabi/prebuilt`。

- 非空：解压并链接 `<dir>/<IDF_TARGET>/libclaw_cabi.a.zst`。
- 空：调用 `cargo +esp build` 从源码构建 `claw-cabi`。

从源码构建示例：

```bash
idf.py -DCLAW_RUST_PREBUILT_DIR= build
```

也可传入另一个 archive root，但目录仍需遵循 `<target>/libclaw_cabi.a.zst` 布局。

### `CLAW_DEBUG`

```bash
export CLAW_DEBUG=1
```

`build-idf-staticlib.sh` 与源码 CMake build 会选择 rich logging。未设置时使用 production logging。该变量不是 Runtime 日志过滤器。

## ESP-Claw Kconfig

`components/common/app_claw/Kconfig` 决定编入哪些 Capability、Lua module 和 UI integration。配置影响 C Capability registry，随后影响 Rust Tool snapshot。

原则：

- “编译进固件”不等于“默认对 LLM 可见”。
- Tool visibility 不是安全边界；Permission 与 Capability validation 仍需执行。
- 依赖具体硬件的选项通常受 Board Manager/SOC capability 约束。
- 修改 Kconfig 后重新运行 `idf.py reconfigure` 或重新生成 board 配置。

完整产品配置由 ESP-Claw 文档站维护，本页只解释与 Rust Runtime 的交界。

## LLM tape 环境变量

`bench/llm-tape` 本身通过 CLI 参数配置 listen/upstream/output。把 Agent 指向 recorder/replayer 时，使用该 Agent 实际读取的 base URL 配置。不要假设 `ANTHROPIC_BASE_URL` 或 `OPENAI_BASE_URL` 会被 `claw-agent-chat` 读取；当前 Host CLI 只读取 `CLAW_LLM_BASE_URL`。
