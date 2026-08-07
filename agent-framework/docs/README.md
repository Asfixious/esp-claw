# Claw Agent Framework 文档

本目录是 `agent-framework/` 的工程文档，面向三类读者：

- **Framework 使用者**：在 Host 上运行 CLI、示例，或把 crates 用到其他 Rust 应用。
- **ESP-IDF 嵌入者**：通过 `claw_agent.h`、CMake 和静态库把 Runtime 接入固件。
- **贡献者与维护者**：修改 Runtime、平台适配、测试或构建系统。

ESP-Claw 设备的烧录、配网、IM 和普通用户操作文档位于仓库根部 `docs/` 文档站，不在这里重复维护。

## 从哪里开始

| 你想回答的问题 | 文档 |
| --- | --- |
| 这是什么，怎样最快跑起来？ | [Getting Started](getting-started.md) |
| 要安装哪些工具？ | [Installation](installation.md) |
| 环境变量、Cargo feature、CMake option 怎么配？ | [Configuration](configuration.md) |
| CLI、Session 和设备接入怎么用？ | [User Guide](user-guide.md) |
| 有哪些可运行 Example？ | [Examples](examples.md) |
| 构建或运行失败怎么办？ | [Troubleshooting](troubleshooting.md) |
| 常见设计问题的答案是什么？ | [FAQ](faq.md) |
| Rust/C API 生命周期和所有权是什么？ | [API 文档](api/README.md) |
| 我要修改代码，从哪里开始？ | [开发指南](development/development-guide.md) |
| 模块为什么这样拆？ | [架构总览](architecture/overview.md) |
| 当初为什么做这个决策？ | [ADR 索引](adr/README.md) |
| 如何提交修改？ | [贡献指南](../CONTRIBUTING.md) |

## 文档结构

```text
docs/
├── getting-started.md
├── installation.md
├── configuration.md
├── user-guide.md
├── examples.md
├── troubleshooting.md
├── faq.md
├── api/
│   ├── README.md
│   ├── rust-api.md
│   └── c-api.md
├── development/
│   ├── development-guide.md
│   ├── build-system.md
│   ├── code-structure.md
│   ├── debugging.md
│   └── testing.md
├── architecture/
│   ├── overview.md
│   ├── data-flow.md
│   ├── concurrency-model.md
│   ├── memory-model.md
│   └── security-model.md
└── adr/
    ├── README.md
    └── 000x-*.md
```

## 文档事实源

文档与代码冲突时，按以下顺序核对：

1. 公开 API：`crates/*/src/lib.rs`、rustdoc、`crates/claw-cabi/include/claw_agent.h`。
2. 行为契约：对应测试和 `snapshots/*.txt`。
3. 构建行为：`Cargo.toml`、`CMakeLists.txt`、`build-idf-staticlib.sh` 与 CI。
4. 稳定设计原则：本目录 `architecture/` 与已接受 ADR。
5. 历史或计划：`rust-migration-summary.md`、`roadmap.md` 和 crate 内 roadmap；它们不是当前 API 的替代品。

## 维护规则

- 命令必须能从文档声明的工作目录直接运行。
- 公共 API 变更必须同步 Rust/C API 文档、示例与 snapshots。
- 平台或工具链约束必须说明“普通固件构建”和“修改 Rust 源码”两条路径的差别。
- 架构文档描述稳定边界，不复制每个实现函数。
- 重大决策新增 ADR；旧 ADR 不重写历史，通过新 ADR supersede。
- 产品级用户行为如果同时出现在根文档站，必须按仓库要求同步中英文页面。
