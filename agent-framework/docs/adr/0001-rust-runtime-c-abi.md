# ADR-0001: Rust 运行时与单一 C ABI

- Status: Accepted
- Date: 2026-08-07

## Context

设备工程已有 ESP-IDF 启动、Event Router、IM、Capability 和大量 C 组件。Agent 运行时同时需要更可靠的资源所有权、异步控制流、类型化错误和可在主机执行的测试。

一次性把所有平台组件改写为 Rust 会扩大迁移范围，并迫使上层 C 代码立即理解 Rust 内部类型。反过来，如果每个 Rust crate 都暴露各自的 C 接口，ABI 数量和跨语言所有权会快速失控。

## Decision

将 Agent 生命周期、会话、模型/工具循环、策略和状态管理放入 Rust workspace。

设备侧保留既有 C/ESP-IDF 集成，通过 `claw-cabi` 暴露单一、显式所有权的 C ABI。C ABI 使用不透明句柄、稳定的错误/事件类型和成对的释放函数；Rust 内部类型与布局不作为 ABI 承诺。

主机应用和测试可以直接调用 Rust API，不需要经过 C ABI。

## Alternatives

### 保持 Agent 运行时全部使用 C

迁移成本最低，但难以获得 Rust 类型系统对所有权、错误和异步状态机的约束，也不能共享新的 Rust 生态实现。

### 一次性把设备工程全部改写为 Rust

边界最统一，但迁移面、硬件验证量和交付风险过大，也会重复实现成熟的平台组件。

### 每个 Rust crate 分别提供 C API

局部集成直接，但会暴露内部模块结构，产生多套句柄、字符串和错误约定，长期兼容成本高。

## Consequences

### Positive

- Agent 核心可在主机和设备之间复用；
- Rust 所有权和类型化错误覆盖最复杂的状态逻辑；
- 既有 ESP-IDF 工程可以渐进迁移；
- C 侧只需维护一个集成边界；
- Rust 内部可以重构而不直接改变 ABI。

### Negative

- 需要维护 FFI 安全、头文件、链接配置和跨语言测试；
- ABI 数据需要编码/复制，不能直接暴露 Rust 引用；
- panic、异步回调和释放责任必须在边界处显式处理；
- 调试需要理解 C 和 Rust 两侧的调用链。

### Follow-up constraints

- `unsafe` 尽量集中在 `claw-cabi` 和 `claw-sys`；
- 新设备集成优先扩展现有 ABI，而不是建立旁路；
- ABI 变更必须同步头文件、调用方、生命周期说明和测试；
- 公开句柄和事件必须有清晰、可执行的释放规则。

