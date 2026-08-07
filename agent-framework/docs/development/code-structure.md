# Code Structure

目录按“稳定责任”而不是“语言语法类型”拆分。一个模块应该拥有自己的状态、规则和错误，同时通过窄接口依赖其他模块。

## 顶层目录

| 目录 | 内容 | 为什么独立 |
| --- | --- | --- |
| `crates/` | Runtime production crates | Domain 与 adapter 可分别测试/复用 |
| `c-legacy/` | 过渡 C adapter | 明确标记非最终架构，避免污染纯 Rust crates |
| `bench/` | Profiling 与 replay 工具 | 不进入固件 dependency closure |
| `cmake/` | IDF/Rust target resolution | 将 build mapping 从业务代码分离 |
| `docs/` | Framework 文档 | 与产品用户站点分工 |
| `snapshots/` | Public Rust API | 独立审查 API 变化 |
| `core/event-router/` | Rust 侧 Router design 研究 | 当前不是固件 Event Router implementation |

## Crate 分层

```text
Application boundary
  claw-cli        claw-cabi
       \           /
        claw-agent
            |
        claw-core
   /    /   |   \      \
 api context memory tool persistence skill permission
   \      \   |    /       /
          claw-interface
                |
             claw-sys
```

实际 Cargo graph 会因 feature 更丰富，但设计方向是上层依赖下层，domain 不反向依赖 application/platform。

## Crate 职责

### `claw-agent`

公开 assembly facade。选择 ToolRegistry、Persistence 与 AgentRuntime 的组合，对外只提供 AgentSystem 生命周期和 Session registry。

不负责：具体 platform API、IM route、Event Router rule、provider wire parsing。

### `claw-core`

拥有 Agent execution、Session actor、Turn/Iteration、mode、context providers、multiagent domain 和 worker orchestration。

不负责：直接打开 socket、直接访问 ESP-IDF、实现 C Capability。

内部按 feature/domain 组织：

- `agent/`：BaseAgent、AgentRun、manager、context providers。
- `session/`：actor、control、stream、state、orchestration。
- `multiagent/`：graph policy、typed commands/effects、tools。
- `runtime/`：worker boundary。
- `resources/`：Agent/prompt/Tool embedded assets。

### `claw-api`

拥有 provider prepare/parse、SSE、chat/media/JSON、retry classification。HTTP transport 由接口注入。

### `claw-interface`

只定义平台能力与 Host implementations/test doubles。它是 dependency inversion seam，不是“所有共享类型”的垃圾桶。

### `claw-sys`

实现 ESP-IDF HTTP、Timer、Thread、Executor、log sink 和少量 C shims。Unsafe 与 SDK ABI 知识应终止在这里。

### `claw-cabi`

负责全局 C Runtime wrapper、C strings/pointers/unions、event conversion 和 C Capability Tool bridge。它不重新实现 Agent policy。

### `claw-tool`

分三层：

- `ToolRegistry`：全局注册和 lifecycle。
- `ToolSet`：单 Agent 可见 projection。
- `ToolRunner`：一次 batch 的同步/异步/detached execution。

Tool Search 读取 metadata，Tool Load 改 projection；两者不替代 Permission。

### `claw-memory`

纯存储对象：Transcript、Profile、Long-term facts。LLM compactor 实现在有 LLM dependency 的 `claw-core`，这里只定义 seam。

### `claw-context`

只处理 Context blocks 的 placement、change detection 和 rendering。它不主动访问 Memory、Skill 或 Tool；provider 负责提供内容。

### `claw-persistence`

提供 typed durable mechanism，不定义 Session/Agent 等 domain DTO 的业务含义。每个 owner 在自己模块中定义 `XxxState` 与 codec。

### `claw-permission`

将 Action 评估为 Allow/Ask/Deny，不执行 Tool、不持有 Session、不决定 UI。

### `claw-skill`

扫描 Skill roots、解析 metadata、生成 catalog、激活 document。脚本执行仍通过具体 Tool/Capability。

### `claw-sandbox`

将虚拟 paths 映射到 injected FS 的显式 real roots，拒绝 root 外访问和只读区写入。

### `claw-log`

提供 plain log、structured trace、格式与 exporter。业务 crate 只发语义 fields，不硬编码可视化布局。

### `claw-utils`

只放无 domain owner 的小型 leaf utilities：typed ID 宏、UTF-8 truncation、stream parts、cooperative tasks。新增内容前先确认不属于某个 domain crate。

### `claw-cli`

Host application boundary。允许 `anyhow` 和文本渲染，不应成为其他 production crates 的依赖。

## C 集成目录

Framework 之外的重要路径：

| 路径 | 责任 |
| --- | --- |
| `components/common/app_claw/` | 固件 lifecycle、config、Capability 注册 |
| `components/claw_capabilities/cap_agent/` | Session RPC 与 Event Router pump |
| `components/claw_modules/claw_cap/` | C Capability registry/dispatch |
| `components/claw_modules/claw_event_router/` | 固件事件规则 |
| `components/claw_modules/claw_im_gateway/` | Channel outbound backend selection |
| `application/edge_agent/` | 产品启动、board、storage、HTTP config |

## 判断新代码放哪里

问以下问题：

1. 谁拥有状态？
2. 谁决定规则？
3. 谁只做适配？
4. 这个类型能否脱离 ESP-IDF 测试？
5. 它是否迫使下层知道上层身份？

如果一个改动同时修改多个层，应保持每层职责清晰：domain 产生 typed effect，adapter 执行平台动作，application 负责 wiring。
