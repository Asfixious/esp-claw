# ADR-0004: 提交按目标芯片构建的压缩静态库

- Status: Accepted
- Date: 2026-08-07

## Context

Agent Rust runtime 最终链接进 ESP-IDF 固件。开发 Rust 代码需要 Rust、`espup` 和相应 target，但许多固件使用者只想构建设备应用，现有 CI/开发环境也不一定都安装 Rust 工具链。

如果每次 `idf.py build` 都现场编译 Rust，会显著增加环境安装和构建时间，并把 Rust 版本漂移带入普通固件构建。只在 Release 附件发布二进制则会使源码 revision 与静态库 revision 难以同步。

## Decision

为支持的 ESP 目标生成 release 静态库，并以 `.a.zst` 形式随源码 revision 保存。常规 ESP-IDF 构建解压并链接对应产物，只需要 `zstd`；修改 Rust 后，维护者使用固定工具链运行 `build-idf-staticlib.sh` 重新生成全部受影响目标。

源码、头文件、构建脚本和预编译库必须在同一变更中保持一致。

## Alternatives

### 每次固件构建都编译 Rust

总能从源码生成，但强制所有使用者安装 Rust/espup，构建时间和失败面更大。

### 从远程 Release 下载静态库

仓库更小，但离线构建、供应链可用性和 revision 对应关系更复杂。

### 提交未压缩 `.a`

流程简单，但显著增加仓库和 checkout 体积。

### 不提交二进制，只支持 Rust 开发环境

维护成本低，但破坏现有 ESP-IDF 使用流程和采用门槛。

## Consequences

### Positive

- 普通固件使用者无需安装 Rust 工具链；
- 构建可离线，并与源码 revision 对齐；
- CI 和发布流程更可预测；
- 压缩降低了预编译产物的仓库占用。

### Negative

- 二进制产物会增加仓库体积；
- 修改 Rust 后必须重建多个芯片目标；
- review 不能直接从二进制 diff 判断内容；
- 工具链、feature 或 linker 设置不一致可能产生陈旧产物。

### Follow-up constraints

- 构建脚本应固定并报告 Rust、ESP target、feature 和 profile；
- CI 应检查预编译产物是否与源码/配置同步；
- 产物来源必须可复现、可追踪；
- 不接受来源不明的静态库替换；
- 支持目标变化时同步更新 README、安装文档和构建矩阵。

