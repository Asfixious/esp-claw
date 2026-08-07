# FAQ

## 这和 ESP-Claw 是什么关系？

ESP-Claw 是完整固件与产品；Claw Agent Framework 是其中的 Rust Agent Runtime。板级驱动、Lua、Event Router、IM、Web 配置和大部分 C Capabilities 位于本目录之外。

## 为什么迁移到 Rust，而不是继续扩展 C Core？

主要目标是明确 ownership、typed errors、可测试平台边界、Session 状态机与 async orchestration，而不是单纯更换语法。核心逻辑现在可用 Host doubles 运行，设备差异集中在 `claw-sys`/`claw-cabi`。

详细原因见[架构总览](architecture/overview.md)和 [ADR-0001](adr/0001-rust-runtime-c-abi.md)。

## 为什么还保留 C？

ESP-IDF 生态、硬件驱动、Lua、Event Router 和既有 Capabilities 已经是稳定 C components。一次性重写会扩大风险。当前设计只迁移 Agent domain，通过一条清晰 ABI 复用其余系统。

## Framework 是 `no_std` 吗？

不是。设备构建使用 Espressif Rust toolchain 为 ESP-IDF 构建 `std` 和 `panic_abort`。部分依赖关闭默认 feature 以控制体积，但 Runtime 不是裸机 `no_std` 项目。

## 普通固件构建需要 Rust 吗？

不需要。仓库提交了四个 target 的 `.a.zst`。只有修改 Rust、切换未提供 archive 的 target 或主动绕过 prebuilt 时才需要 Espressif Rust toolchain。

## 为什么 prebuilt archive 要提交进仓库？

这样 C/ESP-IDF 开发者可以稳定构建固件，不必安装 Rust toolchain；代价是源码、C ABI、IDF ABI 和四个 artifacts 必须通过 `--check` 保持一致。见 [ADR-0004](adr/0004-prebuilt-target-archives.md)。

## 为什么 Release 不开启 LTO？

Rust LTO archive 可能包含 LLVM bitcode，ESP-IDF 的 GCC-based final linker 不能直接消费。当前 profile 使用 `opt-level = "z"`、`codegen-units = 1`、`panic = "abort"`，并明确关闭 LTO。

## 支持哪些芯片？

Committed archives 覆盖 `esp32s3`、`esp32c5`、`esp32p4`、`esp32s31`。支持一块具体开发板还取决于 ESP-Claw Board Manager 配置和其他 C components。

## 支持哪些模型？

`claw-api` 支持 OpenAI-compatible 和 Anthropic-compatible wire format。一个具体 provider/model 是否工作，还取决于它对 Tool call、reasoning、media、structured output 和 SSE 的兼容程度。

## 为什么 Session stream 在 `TurnEnded` 后不关闭？

Session 是长期会话，一条 stream 可承载多个 user Turn 和 detached completion Turn。`TurnEnded` 是局部边界，`Closed` 才是 terminal。见 [ADR-0003](adr/0003-session-event-stream.md)。

## `interrupt` 和 `cancel` 有什么不同？

`interrupt` 协作式终止当前前台 Turn；`cancel` 还会取消 Session 的后台工作。普通用户新输入通常使用 interrupt 语义，避免无意中破坏全部后台任务。

## `submit/append` 和 `respond` 有什么不同？

`submit/append` 创建或排队新的用户 Turn。`respond` 使用 request ID 恢复当前等待输入的 Turn。权限批准和澄清不能用 submit 替代 respond。

## Tool visibility 是安全机制吗？

不是。`tool_search/tool_load` 主要减少 context schema。真正安全边界来自 Tool state、Permission policy、Capability validation、Sandbox 和调用方配置。

## 哪些状态会持久化？

Persistent Session、ID allocator、Tool registry override、Agent recovery state、Transcript、Profile 和 Long-term memory 有各自 durable store。Active future、waker、inbox、临时 Tool visibility、OS handle 和 worker runtime 不持久化。

## 为什么 Memory storage 不直接做 compaction？

Transcript 是事实源，compaction 是“怎样装进当前 LLM context”的视图问题。把二者分开可以保留原始记录，并允许不同模型/窗口使用不同 summary 策略。

## unsafe 在哪里？

多数领域 crates `forbid(unsafe_code)`。必要 unsafe 集中在 `claw-cabi` 的 C pointer/union ownership 和 `claw-sys` 的 ESP-IDF FFI。新增 unsafe 必须写明 invariant，并在安全 wrapper 后终止传播。

## 怎样增加一个新平台？

实现 `claw-interface` 需要的 FS、HTTP/Streaming HTTP、Thread、Executor 和 Timer；不要修改 domain crates 去识别平台。先用 conformance tests 验证，再在应用边界选择具体类型。

## 怎样增加一个新 Tool？

实现 `ToolSpec` 与 sync/async/detached handler，归入有语义的 `ToolGroup`，声明风险 action，添加成功、错误、权限和取消测试。不要仅为 Tool 暴露新的全局状态。

## 迁移总结放在哪里？

[`rust-migration-summary.md`](../rust-migration-summary.md) 记录分支规模与历史。它回答“改了什么”，不是使用或 API 文档的替代品。
