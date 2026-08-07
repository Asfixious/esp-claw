# Contributing to Claw Agent Framework

感谢参与 Claw Agent Framework。这里的改动会同时影响 Host Rust、ESP-IDF 固件、C ABI 和 committed static archives，因此提交前需要明确边界与验证范围。

## 开始之前

1. 阅读[开发指南](docs/development/development-guide.md)与[代码结构](docs/development/code-structure.md)。
2. 涉及 Session、并发、持久化或安全边界时，先阅读对应[架构文档](docs/architecture/overview.md)和 [ADR](docs/adr/README.md)。
3. 搜索现有 issue、测试、design 记录和 git history，确认不是重复问题，也不要仅凭类型名猜测契约。
4. 保持改动最小且职责完整；不要为修一个局部问题顺带重构无关模块。

## 开发环境

Host 开发至少需要 Rust stable、rustfmt 和 Clippy：

```bash
rustup toolchain install stable
rustup component add rustfmt clippy
```

Public API 检查还需要：

```bash
cargo +stable install cargo-public-api --locked
```

设备构建、Python 工具和完整版本要求见[安装指南](docs/installation.md)。

## 选择正确的修改位置

| 修改内容 | 通常归属 |
| --- | --- |
| 对外 AgentSystem 装配 | `claw-agent` |
| Session、Agent loop、multiagent | `claw-core` |
| Provider 协议、SSE、media | `claw-api` |
| 平台无关 FS/HTTP/Thread/Timer 接口 | `claw-interface` |
| ESP-IDF 实现 | `claw-sys` |
| C ABI | `claw-cabi` |
| Tool、Permission、Context、Memory、Skill、Persistence | 对应同名 crate |
| C Capability/Event Router/App 集成 | 仓库根部 `components/` 或 `application/` |

不要让纯领域 crate 直接依赖 `claw-sys`，也不要把 ESP-IDF 类型泄漏进公开 Rust domain API。

## 编码要求

- Library 使用具体、可匹配的 error enum 和 `thiserror`；保留 `#[source]` 或 `#[from]` 链。
- 预期失败返回 `Result`，不要 `panic!`、`unwrap()` 或用假成功吞掉错误。
- `unsafe` 只允许位于必要的平台/FFI 边界，并写清 safety invariant。
- 不在 `.await` 期间持有 `Mutex`/`RwLock` guard。
- 使用 typed ID、enum 和 semantic type，不用字符串模拟状态机。
- 公共类型和 fallible API 写 rustdoc；说明 `# Errors`，unsafe API 说明 `# Safety`。
- 固件热路径避免无界分配、无界队列、无界同步工作和大栈对象。
- 不为测试便利增加没有产品语义的公开 API。

仓库 Rust 细则见 `../.agents/coding_styles/rust.md`。

## 测试

普通 Rust 修改至少运行：

```bash
./check.sh
./harness.sh
```

局部迭代可以先运行：

```bash
cargo test -p <crate-name>
cargo clippy -p <crate-name> --all-targets --all-features -- -D warnings
```

如果公开 API 有意变化：

```bash
./update-public-api-snapshots.sh
git diff -- snapshots/
```

只有确认变化合理时才提交 snapshot。不要用更新 snapshot 掩盖意外 API 扩张。

修改设备路径时还需要：

```bash
./build-idf-staticlib.sh
./build-idf-staticlib.sh --check
```

并至少构建一个受影响目标板。详细矩阵见[测试指南](docs/development/testing.md)。

## 文档

以下变化必须同步文档：

- 新增/删除公开类型、函数、C ABI 字段或 event kind。
- 生命周期、ownership、thread safety、blocking 或 allocation 行为变化。
- Cargo feature、环境变量、CMake option、工具链和支持目标变化。
- 新增 Example、调试工具或排障步骤。
- 重大架构取舍：新增 ADR。

若变化影响 ESP-Claw 普通用户，还需要按仓库规范同步根文档站的英文和简体中文页面。

## Commit 与 Merge Request

推荐使用仓库现有的 Conventional Commit 风格：

```text
feat(claw-tool): add bounded detached completion queue
fix(claw-sys): drain streaming body fairly
docs(agent-framework): add C ABI ownership guide
```

一个 Merge Request 应包含：

- 问题与预期行为。
- 方案、关键边界和未选择的替代方案。
- 测试命令与结果。
- Public API、C ABI、固件 size、持久化格式和兼容性影响。
- 对应文档、snapshot、archive 或 ADR 更新。

## 提交前检查单

- [ ] 改动位于正确模块，没有引入反向依赖或循环依赖。
- [ ] 错误可观察并保留 source，没有静默 fallback。
- [ ] ownership、并发、取消和清理路径有测试。
- [ ] `cargo fmt`、Clippy、Host tests 通过。
- [ ] Public API snapshot 已检查。
- [ ] 需要时已运行设备测试、固件 build 和 archive 校验。
- [ ] 用户/API/开发/架构文档已同步。
- [ ] 日志、fixture、tape 和提交内容不包含密钥或隐私数据。

## 报告问题

报告 bug 时请提供最小复现、版本、目标、feature、完整命令和第一处错误。敏感信息处理与常见诊断步骤见[Troubleshooting](docs/troubleshooting.md)。
