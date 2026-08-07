我觉得我们这一路讨论下来，整个模型已经收敛得比较干净了。最大的变化是：

> **EventRouter 不再是传统 Event Bus，而是整个组件运行时的核心。**

它统一管理 Component 生命周期、RPC 注册与调用，以及由 Event 驱动的数据化 Workflow。

---

# 1. 整体层级

```text
                Application
                     │
          Components (Gateway/Agent/...)
                     │
              EventRouter Runtime
          ┌────────────┴────────────┐
          │                         │
     RpcRegistry             WorkflowRuntime
          │                         │
         RPC                 Workflow JSON
```

RPC 是最底层的调用协议。

EventRouter 建立在 RPC 之上，并由两个平级的内部模块构成：

```text
RpcRegistry
WorkflowRuntime
```

其中：

* `RpcRegistry` 管理所有可调用的 RPC；
* `WorkflowRuntime` 管理并解释执行 Workflow JSON；
* `WorkflowRuntime` 本身也是一个 Component，并在 EventRouter 构造时注册到 `RpcRegistry`。

## Rust package 和 module 边界

当前实现保持一个 library crate：

```text
claw-event-router
├── rpc/          # typed/raw RPC、registry、framing、binary IO
├── workflow/     # Workflow 定义、匹配和解释执行
├── event/        # Event 数据模型与 emit 入口
├── component/    # Component 生命周期与 registration
└── runtime       # EventRouter 组合根
```

这些首先是同一个 crate 内的领域 module，而不是五个独立 crate。这样既可以通过 module visibility 保持依赖方向，也不会过早引入跨 crate error、feature、version 和构建管理成本。

`EventRouter` 是 runtime 的公开组合根类型，不再额外建立一个含义重叠的 `router` module。未来只有当某一层需要被其他 crate 独立依赖、需要独立 feature/平台构建，或需要用 crate boundary 强制单向依赖时，才将它提取成独立 library crate。例如 RPC 真正出现 EventRouter 之外的消费者后，可以独立为 `claw-rpc`。

当前已实现的 RPC API 位于：

```rust
use claw_event_router::rpc::{RpcMethod, RpcRegistry};
```

Registry 必须显式接收 application-lifetime lane storage：

```rust
use std::rc::Rc;
use claw_event_router::rpc::{RpcLaneStorage, RpcRegistry, RpcResult};

fn build_registry() -> RpcResult<Rc<RpcRegistry>> {
    // Host 示例用 Box::leak 模拟 firmware 的 static-cell 放置。
    let lanes = Box::leak(Box::new(RpcLaneStorage::<4, 4096, 8>::new()));
    Ok(Rc::new(RpcRegistry::new(lanes)?))
}
```

---

# 2. RPC

RPC 只负责通信与调用。

RPC 的 transport kernel 只有一种统一调用形态：

```text
BinaryReader → RPC Provider → BinaryWriter
```

输入和输出分别、独立地支持 Unary 和 Stream，因此同一个 `call` 可以表达四种组合：

```text
Unary  → Unary
Unary  → Stream
Stream → Unary
Stream → Stream
```

Rust 组件通常不直接处理这些 bytes，而使用 typed RPC layer：

```text
fixed-layout Request struct
    ↓ IntoBytes，直接写入 request lane
BinaryReader → RPC Provider → BinaryWriter
    ↓ RpcFrame<Response>，直接借用 response lane
&Response
```

`RpcMethod` 同时声明 address、request/response struct 和两侧 cardinality。`RpcRegistry` 不提供 `call_unary` / `call_stream` 两套分发接口；所有 typed 调用统一写成 `client.call::<Method>(input)`，其返回类型由 Method 的 output cardinality 决定。

底层 raw binary API 仍然保留为 `register_raw` / `call_raw`，用于 Lua、C ABI、网络 wire adapter 和自定义 codec。Typed layer 是建立在 raw kernel 上的安全 facade，不会把 registry 与某个业务 struct 耦合。

## Typed method

概念接口：

```rust
use claw_event_router::rpc::{RpcFrame, RpcMethod, Streaming, Unary};
use zerocopy::{Immutable, IntoBytes, KnownLayout, TryFromBytes};

#[repr(C)]
#[derive(Immutable, IntoBytes, KnownLayout, TryFromBytes)]
struct SearchRequest {
    query_len: u16,
    query: [u8; 126],
}

#[repr(C)]
#[derive(Immutable, IntoBytes, KnownLayout, TryFromBytes)]
struct SearchResult {
    title_len: u16,
    title: [u8; 126],
}

struct Search;

impl RpcMethod for Search {
    const ADDRESS: &'static str = "search.query";

    type Request = SearchRequest;
    type Response = SearchResult;
    type Input = Unary;
    type Output = Streaming;
}
```

Typed message 必须是固定布局，不能包含 `String`、`Vec`、指针或引用。`IntoBytes` derive 同时保证没有未初始化 padding；`TryFromBytes` 负责校验并建立借用 view。跨架构协议应使用明确的 byte-order field，而不是假设 native integer endian。

Provider 收到 `RpcFrame<Request>`，业务代码通过 `view()` 借用 request；返回值仍是 owned fixed-layout struct 或 `RpcStream<struct>`：

```rust
registry.register_typed::<Search, _>(|context, request: RpcFrame<SearchRequest>| async move {
    let request: &SearchRequest = request.view()?;
    Ok(results)
})?;

let mut results = client.call::<Search>(request)?;
while let Some(frame) = results.next().await {
    consume(frame?.view()?);
}
```

四种组合仍然通过同一个 `call` 表达：

| Input | Output | Handler input | Handler output | Client result |
| --- | --- | --- | --- | --- |
| Unary | Unary | `RpcFrame<Request>` | `Response` | `RpcUnaryCall<Response>` → `RpcFrame<Response>` |
| Unary | Stream | `RpcFrame<Request>` | `RpcStream<Response>` | `RpcStream<RpcFrame<Response>>` |
| Stream | Unary | `RpcStream<RpcFrame<Request>>` | `Response` | `RpcUnaryCall<Response>` → `RpcFrame<Response>` |
| Stream | Stream | `RpcStream<RpcFrame<Request>>` | `RpcStream<Response>` | `RpcStream<RpcFrame<Response>>` |

Typed call 是 self-driving 的。poll 返回的 future/stream 时，同一个 driver 会公平推进 lane acquisition、request encoder、provider future 和 response decoder，不要求调用者额外 spawn 或 `join!` 一个 raw call future。

## Static lanes、framing 和限制

`RpcLaneStorage<N, M, Q>` 在初始化时确定 RPC transport 的 payload 上限：

```text
N lanes
└── each lane
    ├── request:  align(16) [u8; M]
    └── response: align(16) [u8; M]

Q fixed root-call waiter slots
```

`N` 是同时 active 的 call/retained response 数量；每个 lane 有独立的 request/response buffer，所以 payload 静态占用约为 `N * 2 * M`，再加 lane metadata 和 `Q` 个 waiter slot。`M` 同时是一个 typed frame 的硬上限。Streaming 不会占用更大的 buffer，而是逐 frame 复用同一个 lane，并通过 `Pending` 提供 backpressure。

根调用在 `N` 个 lane 全部占用时进入固定 waiter table，按 ticket 顺序等待；超过 `Q` 返回 `LaneWaiterCapacityExceeded`。嵌套调用不能在持有外层 lane 时无限等待：若没有空 lane，立即返回 `NestedLaneExhausted`，避免 `N` 条调用链互相持有 lane 形成死锁。取消 active call 会释放 lane；取消 waiter（包括已经获得 reservation、尚未再次 poll 的 waiter）会把 reservation 交给下一个 waiter。

Lane-backed typed request 通过 `IntoBytes::as_bytes()` 直接复制到 request lane；接收端和 response 端不反序列化，而是由 `RpcFrame<T>` 持有 frame lease，并通过 `TryFromBytes::try_ref_from_bytes()` 返回指向 lane 的 `&T`。frame boundary 由 lane metadata 表达，不创建中间 `Vec<u8>`，response payload 也不复制。

`RpcFrame<T>` drop 前，对应 pipe 不会被覆盖；drop 时 frame 才被消费并唤醒等待的 writer/reader。Unary response frame 还会保留整个 lane，因此调用者应在用完 view 后尽快 drop frame。若需要跨越后续调用长期保存结果，应把所需字段复制到自己的 fixed-layout storage，再释放 frame。Streaming 同样要求消费并释放当前 frame 后，下一帧才能复用该方向的 buffer。

通过普通 raw byte reader/writer 调用 typed provider 时，兼容路径使用：

```text
u32 little-endian payload length | fixed-layout message bytes
```

这个 fallback 会先收集一个 frame，再用 `TryFromBytes::try_read_from_bytes()` 得到 owned message；真正的 lane path 才提供借用式 zero-copy view。

Unary 必须恰好包含一个 frame，Streaming 包含零个或多个 frame 并以 EOF 结束。每个 Method 分别声明 request/response 最大 frame size（默认 4096 bytes）；typed provider 注册和 typed client 调用都会在 acquisition 和 payload IO 之前检查两侧限制均不大于 `M`。消息大小超过 Method limit 时返回 `FrameTooLarge`。消息 alignment 大于 lane 保证的 16 bytes 时，在注册或调用前返回 `MessageAlignmentExceedsLane`。

Typed message 和 transport payload 已经完全固定布局：消息不能携带 `String`、`Vec` 等动态字段，response view 直接借用 lane。Registry entries、boxed provider/future/IO handle 等 RPC 控制对象目前仍会使用 heap；若固件 profile 要求整个 RPC runtime 完全无 heap，还需要继续把这些控制对象放入 object pool。

Registry 在注册 typed provider 时保存 method descriptor。Typed client 会在开始任何 payload IO 前校验 method marker、request/response type、两侧 cardinality 和 limits，防止同一 address 上发生错误解析。Raw client 不做 typed signature 校验，因为它是显式选择的 escape hatch。

Zerocopy 是 typed RPC 的唯一 codec；固定布局、alignment、padding、bit validity 和 frame lease 都是公开契约的一部分。需要动态数据模型或其他 wire format 的调用方应显式使用 raw kernel，并在 adapter 中定义自己的 codec。

RPC 不关心：

* Event
* Workflow
* Scheduler
* Gateway
* Agent
* Component 的具体类型

RPC 是系统中 Component 之间唯一的主动调用协议。

例如：

```text
Agent → Search
Search → HTTP
Agent → Lua
Agent → Scheduler
WorkflowRuntime → Agent
```

这些全部都是 RPC。

## Unary

一次请求对应一次响应：

```text
Agent → scheduler.create → ScheduleId
Search → http.request → HttpResponse
Agent → lua.execute → LuaResult
```

## Stream

用于持续产生数据或传输流式内容：

```text
Agent stream
HTTP response body
文件
音频
视频
大段文本
实时数据流
```

只要底层对象实现统一的 `BinaryReader` 和 `BinaryWriter`，就可以接入 RPC。Unary 与 Stream 不属于不同的调用类型，只由输入输出何时到达 EOF 决定。

Stream 不只是“大对象传输”，也可以用于小块但持续产生的数据，例如 LLM token、日志和实时消息。

---

# 3. Component

整个系统只有一种运行对象：

```text
Component
```

例如：

```text
Gateway
Agent
Scheduler
WorkflowRuntime
Search
HTTP
Lua
Storage
```

代码模型中不存在：

```text
ActiveComponent
PassiveComponent
```

所谓 active 和 passive，只是 Component 在行为上的区别，不进入类型系统。

例如：

* Scheduler 的 `run()` 会持续等待 timer；
* Gateway 的 `run()` 会持续等待外部消息；
* HTTP 可能只在收到 RPC 调用时执行；
* WorkflowRuntime 主要在收到 Event 或 workflow RPC 调用时执行。

它们依然都实现同一个 Component 生命周期。

---

# 4. Component 生命周期

所有 Component 都遵循统一生命周期：

```text
register
    ↓
run
    ↓
unregister
```

概念接口：

```rust
use core::{future::Future, pin::Pin};

pub type ComponentFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), ComponentError>> + 'a>>;

trait Component {
    fn register(
        &mut self,
        ctx: &mut RegisterContext<'_>,
    ) -> Result<(), ComponentError>;

    fn run<'a>(
        &'a mut self,
        ctx: RunContext,
    ) -> ComponentFuture<'a>;

    fn unregister(
        &mut self,
        ctx: &mut UnregisterContext<'_>,
    ) -> Result<(), ComponentError>;
}
```

这里的设计原则是：

* `register()` 和 `unregister()` 是同步、短小、不可阻塞的装配操作；
* `run()` 是 Component 唯一的顶层异步生命周期；
* EventRouter 以 Component 为生命周期管理单位，而不是管理 Component 内部零散的 task；
* Component 内部如果需要多个并发循环，应当在 `run()` 中使用 `select!`、`join!` 等结构化并发自行组织。

## register

负责：

* 注册 RPC；
* 初始化内存中的资源和状态；
* 声明自己可能产生的 Event；
* 检查名称、schema 和 visibility 冲突；
* 建立 RPC facade、mailbox sender 等对外句柄。

`register()` 不进入长期运行循环，也不执行网络连接、异步设备初始化等需要等待的工作。这类异步启动逻辑应当放进 `run()`。

EventRouter 应记录 Component 在注册阶段创建的所有 registration，并在卸载时自动清理，避免 Component 自己逐个注销导致遗漏。

## run

`run()` 负责 Component 的完整异步运行期，例如：

```text
等待 timer
等待 socket
等待外部消息
处理 mailbox command
emit Event
调用 RPC
推进内部状态
响应 shutdown
```

Component 可以自然使用：

```rust
timer.await;
socket.read(...).await;
router.call(...).await;
router.emit(...).await;
```

对于没有持续后台逻辑的 Component，`run()` 可以只等待 Router shutdown。这个长期 Pending future 只产生很小的运行时开销，但换来了统一、清晰的生命周期模型。

`run()` 持有 `&mut self`，因此在运行期间它是 Component 可变状态的唯一 owner。对于也需要访问同一份状态的 RPC，推荐使用 mailbox：

```text
RPC Provider
    ↓ send command
Component::run()
    ↓ receive command
    ↓ mutate component state
```

mailbox 只是有状态 Component 的内部实现模式，不是 EventRouter 的全局要求。无状态 RPC 或持有独立 cloneable handle 的 RPC provider 可以直接执行。

## unregister

`unregister()` 只负责同步拆除：

* 清理本地资源；
* 移除残余 registration metadata；
* 完成不需要等待的最终收尾。

需要异步完成的 graceful shutdown、连接关闭和 flush，应当在 `run()` 收到 shutdown 后、返回之前完成。

动态卸载时，EventRouter 必须先通知 `run()` 退出并等待其完成，之后才能重新获得 `&mut Component` 并调用 `unregister()`。

---

# 5. RpcRegistry

`RpcRegistry` 管理：

```text
RPC Address
    ↓
RPC Provider
```

职责包括：

* 注册；
* 注销；
* RPC 地址解析；
* 统一 binary reader / writer dispatch；
* 分组；
* visibility；
* schema；
* catalog；
* 调用上下文传播；
* 直接自调用检查。

例如：

```text
RpcRegistry
├── agent
├── search
├── http
├── lua
├── scheduler
└── workflow
```

对 RpcRegistry 来说，它不关心 RPC 背后的实现到底是：

* Agent；
* WorkflowRuntime；
* Lua；
* HTTP；
* Scheduler。

它只知道：

```text
RpcAddress → RpcProvider
```

---

# 6. Workflow 和 WorkflowRuntime

这里必须严格区分两个概念。

## Workflow

Workflow 不是 Rust 对象，也不是 Component。

Workflow 是纯数据定义：

```text
Workflow = JSON
```

例如：

```json
{
  "id": "gateway_to_agent",
  "match": {
    "type": "gateway.message.received"
  },
  "steps": [
    {
      "call": "agent",
      "mode": "stream",
      "input": "$event.payload"
    }
  ]
}
```

Workflow JSON 可以描述：

* Event 匹配条件；
* RPC 调用；
* 输入映射；
* 顺序执行；
* 条件分支；
* 循环；
* 后续可能支持的并行和 join。

Workflow 本身不执行任何东西，它只是数据。

## WorkflowRuntime

`WorkflowRuntime` 才是 Component。

它负责：

* 加载和管理 Workflow JSON；
* 接收 Event；
* 匹配符合条件的 Workflow；
* 创建 Workflow execution；
* 解释执行 Workflow JSON；
* 维护 execution context；
* 通过 RpcRegistry 调用 RPC；
* 返回 Workflow 的 unary 或 stream 输出。

结构：

```text
WorkflowRuntime
├── Workflow JSON definitions
├── Event matcher
├── Workflow interpreter
├── Execution state
└── RPC step executor
```

---

# 7. WorkflowRuntime 与 RpcRegistry 的关系

`RpcRegistry` 和 `WorkflowRuntime` 是 EventRouter 内部的两个平级模块。

```text
EventRouter
├── RpcRegistry
└── WorkflowRuntime
```

但它们在运行时双向连接。

## WorkflowRuntime 注册到 RpcRegistry

EventRouter 构造时，将 WorkflowRuntime 对外暴露的 RPC 注册到 RpcRegistry：

```text
WorkflowRuntime
    │ implements workflow RPC
    ▼
RpcRegistry.register("workflow", ...)
```

所以其他 Component 可以通过 RPC 直接启动 Workflow：

```text
Agent
    ↓ call workflow RPC
RpcRegistry
    ↓
WorkflowRuntime
    ↓
执行指定 Workflow JSON
```

## WorkflowRuntime 通过 RpcRegistry 调用其他 RPC

执行 Workflow JSON 时：

```text
WorkflowRuntime
    ↓
RpcRegistry.call("agent")
RpcRegistry.call("lua")
RpcRegistry.call("search")
RpcRegistry.call("scheduler")
```

因此形成：

```text
               ┌───────────────┐
               │  RpcRegistry  │
               └──────▲───┬────┘
                      │   │
       register as RPC│   │call RPCs
                      │   ▼
               ┌─────────────────┐
               │ WorkflowRuntime │
               └─────────────────┘
```

这不是层级依赖错误，而是 EventRouter 在构造阶段建立的运行时组合关系。

---

# 8. Event

Event 不是传统 Pub/Sub 消息。

EventRouter 没有：

* Handler；
* Subscriber；
* Callback；
* Binding Table；
* Route Table。

Event 的定义是：

> **WorkflowRuntime 的外部输入，以及 Workflow JSON 的触发源。**

Event 本身也是数据，例如：

```json
{
  "type": "gateway.message.received",
  "source": "telegram",
  "payload": {
    "conversation_id": "chat-123",
    "text": "hello"
  }
}
```

调用：

```rust
router.emit(event).await?;
```

`emit()` 明确是 fire-and-forget：它只等待 EventRouter 接管 Event，不等待 matcher、Workflow execution 或任何 RPC step 完成。后续执行失败由 WorkflowRuntime 自己记录、处理或产生新的失败 Event。

实际执行路径是：

```text
Event
    ↓
WorkflowRuntime
    ↓
匹配 Workflow JSON
    ↓
创建 Workflow execution
    ↓
通过 RpcRegistry 执行 RPC steps
```

因此：

```text
emit(event)
    ↓
WorkflowRuntime
    ↓
Workflow JSON
    ↓
RPC
```

---

# 9. Event 与 Workflow 的关联方式

Event 和 Workflow 之间不存在静态绑定。

不是：

```text
Event Type → 固定 Workflow
```

也不是由 Rust 代码写：

```rust
workflow!(
    trigger = GatewayMessageReceived,
)
```

Workflow 是 JSON，匹配规则也属于 Workflow JSON 数据。

例如：

```json
{
  "id": "gateway_to_agent",
  "match": {
    "type": "gateway.message.received",
    "source": "telegram"
  },
  "steps": [
    {
      "call": "agent",
      "input": "$event.payload"
    }
  ]
}
```

运行时：

```text
emit Event
    ↓
WorkflowRuntime 查询候选 Workflow JSON
    ↓
根据 match 字段匹配 Event
    ↓
启动匹配成功的 Workflow
```

这样：

* Event 不知道 Workflow；
* Workflow 不知道 Event producer；
* Gateway 不知道 Agent；
* EventRouter 不需要维护独立的 Event-to-Workflow 映射表；
* Workflow 可以动态增加、删除和修改；
* 一个 Workflow 可以匹配多种 Event；
* 一个 Event 也可以匹配一个或多个 Workflow。

所以 Event 与 Workflow 是通过数据驱动的 matcher 解耦关联，而不是通过代码绑定。

---

# 10. EventRouter

EventRouter 是整个 Runtime 的组合根。

内部结构：

```text
EventRouter
├── RpcRegistry
├── WorkflowRuntime
└── Components
```

它负责：

* 构造 RpcRegistry；
* 构造 WorkflowRuntime；
* 向 WorkflowRuntime 提供 RpcRegistry 调用能力；
* 将 WorkflowRuntime 注册到 RpcRegistry；
* 动态注册其他 Components；
* 管理 Component 生命周期；
* 驱动 Component 的 `register/run/unregister`；
* 对外暴露 RPC call 和 Event emit；
* 管理系统级 shutdown。

构造过程：

```text
EventRouter::new()
├── create RpcRegistry
├── create WorkflowRuntime
├── connect WorkflowRuntime to RpcRegistry
├── register WorkflowRuntime RPC into RpcRegistry
└── accept dynamic Components
```

---

# 11. 执行模型

EventRouter 本身不是 Platform Executor。

它不负责：

* 创建线程；
* 创建 OS task；
* 跨核调度；
* 平台 reactor；
* 通用 task spawning。

EventRouter 对外表现为一个可异步运行的 Runtime：

```rust
event_router.run().await
```

它由外部 Executor 驱动：

```text
Embassy
Tokio
Custom Executor
FreeRTOS-backed executor
WASM executor
        ↓
EventRouter.run()
```

所以结构是：

```text
External Executor
    ↓
EventRouter Runtime
    ↓
Components + WorkflowRuntime + RpcRegistry
```

---

# 12. Component 驱动

EventRouter 以“一份 Component 对应一个顶层 `run()` future”为基本模型：

```text
ComponentId
    ↔ one Component
    ↔ one run future
```

EventRouter 只管理 Component 的顶层生命周期，不向 Component 暴露 `attach_task/unattach_task` 作为一等 API。

如果一个 Component 内部同时需要处理多个异步来源，应当在自己的 `run()` 内组织：

```rust
Box::pin(async move {
    loop {
        select! {
            command = self.commands.next() => {
                self.handle_command(command?).await?;
            }

            _ = self.next_timer() => {
                self.trigger_due(&ctx).await?;
            }

            _ = ctx.shutdown() => {
                return Ok(());
            }
        }
    }
})
```

这保持了清晰的职责边界：

```text
EventRouter 管理 Component 生命周期
Component 管理自己的内部并发
```

EventRouter 不需要理解 Component 内部具体在等待 timer、socket、mailbox 还是其他异步源。

如果 Component 需要执行：

```text
CPU-heavy computation
blocking filesystem operation
blocking C API
compression
large parsing task
```

如何 offload、使用什么 worker、如何设置队列和背压，都是该 Component 自己的职责。

EventRouter 只要求：

> Component 的 `run()` 必须是 cooperative 的，不能长时间阻塞负责驱动 EventRouter 的线程。

---

# 13. 动态加载、卸载与异步驱动

Caller 面向的是 EventRouter 的动态 Component 管理 API：

```rust
let id = router.load(Box::new(component)).await?;
router.unload(id).await?;
```

`Component::register()` 和 `Component::unregister()` 本身仍然是同步方法；`EventRouter::load()` 和 `EventRouter::unload()` 是异步操作，因为它们负责完整的运行时生命周期。

## 动态加载

```text
load(component).await
    ↓
分配 ComponentId
    ↓
Component::register()
    ↓
原子提交 RPC registrations
    ↓
创建并插入 Component::run() future
    ↓
标记 Running
    ↓
返回 ComponentId
```

注册过程应使用 transaction/registration token。若中途失败，EventRouter 自动回滚已经创建的 RPC、schema 和其他 registration。

## 动态卸载

```text
unload(component_id).await
    ↓
标记 Stopping，拒绝新的 RPC
    ↓
通知 run() shutdown
    ↓
等待 run() 完成异步清理并返回
    ↓
Component::unregister()
    ↓
自动移除 RPC registrations
    ↓
删除 Component entry
```

EventRouter 已经在外部 Executor 中运行，因此 caller 不直接并发修改 Router 内部状态。`load/unload` 请求通过 Router command queue 提交，并由 `EventRouter::run()` 串行处理。

```text
Caller
    ↓ load/unload command
EventRouter::run()
    ↓ serial mutation
Component table + RpcRegistry + run task set
```

## 动态异构 Future

不同 Component 的 `run()` 会产生不同的 Future 类型，因此 EventRouter 内部需要类型擦除后的动态 task set。

V1 采用架构优雅优先的实现：

```rust
pub type ComponentFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), ComponentError>> + 'a>>;
```

EventRouter 内部可使用类似 `FuturesUnordered` 的 ready-queue task set，只 poll 已被唤醒的 Component future，而不是每次扫描全部 Component。

外部 Executor 仍然只驱动一个：

```text
EventRouter::run()
```

EventRouter 内部的动态 task set 是 Component 生命周期的调度实现细节，不向 Component 暴露通用 spawn/attach API，也不负责线程或平台级调度。

---

# 14. RPC 调用规则

RPC 允许间接形成环：

```text
A
↓
B
↓
A
```

禁止直接自调用：

```text
A
↓
A
```

也就是：

> 一个 RPC endpoint instance 不能同步地通过 RpcRegistry 再调用自身。

但是：

```text
workflow
    ↓
agent
    ↓
workflow
```

是允许的。

因为 Workflow 调用关系来自显式的 Workflow JSON，是否形成循环、循环多少次、什么时候退出，都属于 WorkflowRuntime 的执行语义。

RpcRegistry 不负责：

* 全局 cycle detection；
* 强制 DAG；
* 检查完整调用栈是否重复。

它只禁止直接 self-call。

---

# 15. Gateway

Gateway 是一个普通 Component。

它负责：

```text
外部消息协议
    ↓
标准化 Event
    ↓
router.emit(event)
```

Gateway 不认识 Agent，也不硬编码：

```text
收到消息 → 调用 Agent
```

它只产生：

```json
{
  "type": "gateway.message.received",
  "payload": {
    "conversation_id": "chat-123",
    "text": "hello"
  }
}
```

WorkflowRuntime 匹配对应 Workflow JSON：

```json
{
  "id": "gateway_to_agent",
  "match": {
    "type": "gateway.message.received"
  },
  "steps": [
    {
      "call": "agent",
      "mode": "stream",
      "input": "$event.payload"
    }
  ]
}
```

因此：

```text
Gateway
    ↓ emit Event
WorkflowRuntime
    ↓ match Workflow JSON
Agent RPC
```

Gateway 与 Agent 完全解耦。

---

# 16. Scheduler

Scheduler也是普通 Component 和 RPC Provider。

它的职责是：

```text
Time Condition
    ↓
Trigger One Action
```

例如：

```text
到达某个时间
    ↓
call workflow RPC
```

或者：

```text
到达某个时间
    ↓
emit Event
```

Scheduler 不负责：

* DAG；
* 多步骤执行；
* 条件分支；
* Workflow 状态；
* 多节点编排。

这些全部属于 WorkflowRuntime。

Scheduler 只负责“什么时候触发一个目标”。

---

# 17. 最终调用关系

Gateway 消息场景：

```text
Gateway
    │
    │ emit Event
    ▼
WorkflowRuntime
    │
    │ match Workflow JSON
    ▼
Workflow Execution
    │
    │ call RPC
    ▼
Agent
    │
    ├────► Search
    │         │
    │         └────► HTTP
    │
    ├────► Lua
    │
    └────► Scheduler
```

Scheduler 到期：

```text
Scheduler
    │
    │ timer expires
    ▼
emit Event / call workflow RPC
    │
    ▼
WorkflowRuntime
    │
    ▼
RPC calls
```

直接调用场景：

```text
Agent → RpcRegistry → Search
Search → RpcRegistry → HTTP
Agent → RpcRegistry → Lua
Agent → RpcRegistry → Scheduler
Agent → RpcRegistry → WorkflowRuntime
```

---


# 18. Component Runtime 的最终决策

Component Runtime 采用以下明确约束：

```text
一份 Component
    ↓
一个顶层 run Future
    ↓
EventRouter 负责加载、停止和卸载
```

不采用 `attach_task/unattach_task` 作为公开 Component API。原因是它会把 Component 内部并发和 task supervision 泄漏给 EventRouter，使一个 Component 对应 0..N 个独立生命周期，增加失败、退出、卸载和资源共享语义。

对于有状态 Component：

```text
RPC facade → mailbox → Component::run() → mutable state
```

对于无状态或具有独立 handle 的 Component，RPC 可以直接执行，不必强制经过 mailbox。

V1 接受 boxed future、动态分发和 mailbox hop 的成本，以换取：

* 清晰的所有权；
* 统一的生命周期；
* 动态加载和卸载；
* 自然的 async 编程；
* 较少的 unsafe 和 executor 定制。

---

# 19. 最终概念定义

## RPC

系统底层统一的 unary / stream 调用协议。

## RpcRegistry

负责 RPC 的注册、注销、寻址、dispatch、visibility、schema 和 catalog。

## Component

系统中唯一的运行对象，遵循：

```text
register → run → unregister
```

`register/unregister` 是同步装配与拆除；`run()` 返回 boxed `ComponentFuture`，代表 Component 唯一的顶层异步生命周期。Component 可以提供 RPC，也可以 emit Event；有状态 RPC 可以通过 mailbox 与 `run()` 中独占的状态交互。

## Workflow

描述 Event 匹配和 RPC 执行逻辑的 JSON 数据。

Workflow 本身不是 Component，也不是 Rust handler。

## WorkflowRuntime

EventRouter 内部的 Component，负责管理和解释 Workflow JSON、匹配 Event，并通过 RpcRegistry 执行其中的 RPC steps。

## Event

WorkflowRuntime 的外部输入。Event 不绑定 Workflow，也不是传统 Pub/Sub 消息。

## EventRouter

整个运行时的组合根，由平级的 RpcRegistry 和 WorkflowRuntime 构成，并负责所有 Component 的生命周期与异步运行。

最终可以浓缩成一句话：

> **EventRouter 是一个由外部 Executor 驱动的组件运行时。所有 Component 通过 RpcRegistry 提供和调用 RPC，通过 emit 产生 Event；WorkflowRuntime 根据 JSON 中的数据化匹配规则选择 Workflow，并解释执行其中的 RPC 调用流程。**
