# ADR-0002: 注入式平台边界

- Status: Accepted
- Date: 2026-08-07

## Context

Agent 需要 HTTP、存储、时钟、日志和系统能力。设备使用 ESP-IDF，主机测试和 CLI 使用不同实现。如果领域 crate 直接调用 ESP-IDF API，核心逻辑将无法在主机独立测试，错误注入也会变得困难。

全局函数和单例虽然接入简单，但会隐藏依赖、增加测试间共享状态，并让初始化顺序成为隐式约束。

## Decision

在 `claw-interface` 中定义小而面向能力的平台端口 trait。领域和策略 crate 依赖这些 trait；`claw-sys`、主机 adapter 或测试 double 提供实现，并在组合根注入。

平台相关数据在 adapter 中转换成领域类型和类型化错误。领域 crate 不直接依赖 ESP-IDF 头文件、句柄或错误码。

## Alternatives

### 领域 crate 直接调用 ESP-IDF

调用路径短，但平台耦合扩散，主机测试和复用困难。

### 使用全局函数表或进程级单例

C 集成方便，但依赖不可见、初始化脆弱，并发测试容易互相影响。

### 条件编译两套领域实现

可以针对平台优化，但会复制业务逻辑，长期产生语义漂移。

## Consequences

### Positive

- 核心行为可在主机快速测试；
- 可精确注入超时、断网、损坏存储等失败；
- 平台升级集中在 adapter；
- crate 依赖方向更清楚；
- 便于未来增加新芯片或主机实现。

### Negative

- trait、adapter 和错误映射增加样板代码；
- 过度抽象可能隐藏平台特有能力或产生动态分发成本；
- 生命周期和 `Send`/`Sync` 约束需要在接口设计时明确。

### Follow-up constraints

- trait 应保持小而稳定，避免建立“万能平台对象”；
- 只有确实需要替换或隔离的平台能力才进入接口层；
- adapter 不承载 Agent 策略；
- 测试 double 必须保持关键错误语义，不能只实现成功路径。

