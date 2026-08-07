# Architecture Decision Records

ADR 用于记录影响多个模块、预计长期存在且难以仅从代码中还原原因的架构决策。

## 当前记录

| ADR | 状态 | 决策 |
| --- | --- | --- |
| [0001](0001-rust-runtime-c-abi.md) | Accepted | Agent 运行时迁移到 Rust，通过单一 C ABI 接入既有设备工程 |
| [0002](0002-injected-platform-boundaries.md) | Accepted | 领域 crate 依赖注入式平台端口，而非直接调用 ESP-IDF |
| [0003](0003-session-event-stream.md) | Accepted | 会话使用跨多个 Turn 的长生命周期事件流 |
| [0004](0004-prebuilt-target-archives.md) | Accepted | 提交按芯片构建的压缩 Rust 静态库，支持无 Rust 工具链的固件构建 |
| [0005](0005-typed-persistence.md) | Accepted | 持久状态由领域 owner 定义类型与版本，持久化层负责可靠提交 |

## 状态

- **Proposed**：正在讨论，尚不能作为实现约束；
- **Accepted**：当前实现应遵循；
- **Superseded**：已被另一 ADR 替代，原文保留；
- **Deprecated**：不再推荐，但可能仍有兼容代码；
- **Rejected**：讨论过但未采用。

## 新增 ADR

文件名使用 `NNNN-short-title.md`，正文至少包含：

```markdown
# ADR-NNNN: 标题

- Status: Proposed
- Date: YYYY-MM-DD

## Context
## Decision
## Alternatives
## Consequences
```

ADR 记录“为什么”和长期影响，不替代实现文档、Issue 或提交说明。修改已 Accepted 的决策时，优先新增 ADR 并把旧记录标为 Superseded。

