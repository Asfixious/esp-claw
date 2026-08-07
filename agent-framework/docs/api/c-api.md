# C API

C API 位于 [`crates/claw-cabi/include/claw_agent.h`](../../crates/claw-cabi/include/claw_agent.h)。它是 ESP-IDF 应用访问 Rust Agent Runtime 的唯一公开 ABI。

## 生命周期

```text
claw_agent_init
  -> claw_agent_link_api (可选/可重复)
  -> claw_agent_start
  -> Session operations
  -> claw_agent_stop
  -> claw_agent_deinit
```

| API | blocking | 作用 |
| --- | --- | --- |
| `claw_agent_init` | 是 | 构造 stopped Runtime、恢复状态、注册 C Capability Tools |
| `claw_agent_link_api` | 是 | 绑定或替换某个 purpose 的 LLM API |
| `claw_agent_start` | 是 | 激活 tools 并开放 Session API |
| `claw_agent_stop` | 是 | 停用 tools，保留 Runtime/Sessions/bindings |
| `claw_agent_deinit` | 是 | 释放全局 Runtime |

Lifecycle 应由应用中的单一 owner 串行管理。不要从 ISR 调用这些函数；它们会分配内存、使用锁、文件系统和 worker。

## 初始化

```c
#include "claw_agent.h"

claw_agent_config_t config = {
    .api_key = NULL,
    .backend_type = NULL,
    .model = NULL,
    .base_url = NULL,
    .persistence_dir = "/data/agent",
    .skills_root_dir = "/data/skills",
    .system_skills_root_dir = "/system/skills",
};

ESP_ERROR_CHECK(claw_agent_init(&config));

claw_agent_api_config_t api = {
    .api_key = "...",
    .backend_type = "openai_compatible",
    .model = "...",
    .base_url = "https://example.com/v1",
};

ESP_ERROR_CHECK(claw_agent_link_api(
    &api,
    CLAW_AGENT_API_PURPOSE_ROOT_AGENT,
    true));
ESP_ERROR_CHECK(claw_agent_start());
```

初始四个 API 字段必须“全部有效”或“全部为空”。`persistence_dir` 必须存在语义且字符串有效；Skill roots 可为 `NULL`。

输入字符串只在调用期间借用，Runtime 会按需要复制。调用方仍负责原始配置 buffer 的正常生命周期直到函数返回。

## Session API

| API | 行为 |
| --- | --- |
| `session_create` | 创建 persistent 或 ephemeral numeric ID |
| `session_list` | 获取 live IDs；容量不足返回所需 count |
| `session_open` | 打开唯一 event stream |
| `session_submit` | 将普通输入加入 FIFO |
| `session_respond` | 回答当前 `INPUT_REQUESTED` |
| `session_interrupt` | 协作式中断前台 Turn |
| `session_cancel` | 取消前后台工作 |
| `session_receive` | 每次接收一个 event，可 timeout/non-blocking |
| `session_close` | 关闭 stream，保留 Session ID |
| `session_delete` | 删除 Session state |

`session_id` 必须非零。Create、open、submit 等操作只能在 started Runtime 上执行。

## 接收事件

```c
uint32_t session_id = 0;
ESP_ERROR_CHECK(claw_agent_session_create(
    CLAW_AGENT_SESSION_PERSISTENCE_PERSISTENT,
    &session_id));
ESP_ERROR_CHECK(claw_agent_session_open(session_id));
ESP_ERROR_CHECK(claw_agent_session_submit(session_id, "hello"));

bool closed = false;
while (!closed) {
    claw_agent_event_t event = {0};
    esp_err_t err = claw_agent_session_receive(session_id, &event, 1000);
    if (err == ESP_ERR_TIMEOUT) {
        continue;
    }
    ESP_ERROR_CHECK(err);

    switch (event.kind) {
    case CLAW_AGENT_EVENT_KIND_OUTPUT_DELTA:
        printf("%s", event.data.text_delta.text);
        break;
    case CLAW_AGENT_EVENT_KIND_INPUT_REQUESTED:
        /* Save request_id and call session_respond from the input path. */
        break;
    case CLAW_AGENT_EVENT_KIND_CLOSED:
        closed = true;
        break;
    default:
        break;
    }

    claw_agent_event_free(&event);
}
```

### Event ownership

- `kind` 是 union tag，只能读取对应 member。
- `receive == ESP_OK` 后 event 中的 owned strings 由 C 调用方暂时持有。
- 每个成功 event 必须 exactly once 调用 `claw_agent_event_free`。
- Free 后 event 被重置，任何内部字符串指针都失效。
- 忽略的 delta、boundary 和 error event 也必须 free。
- 不要复制 struct 后分别 free 两个副本。

## Event 顺序

一次普通 Turn 的核心顺序：

```text
TURN_STARTED
  ITERATION_STARTED
    REASONING_DELTA* / REASONING_END
    OUTPUT_DELTA* / OUTPUT_END
    TOOL_CALL* / TOOL_CALLS_END
  ITERATION_ENDED
TURN_ENDED
```

`TURN_ENDED` 不关闭 Session stream。调用方应继续读取后续 user Turn 或 detached completion；`CLOSED` 才是 terminal。

`TOOL_CALL` 暴露完整 provider ID、name 和 arguments JSON，但当前 C event 不暴露 Tool result body。

`USAGE` 只在 `cache_profile` feature 下存在，未报告的 counter 使用 `UINT64_MAX`。

## Respond

收到 `INPUT_REQUESTED` 时：

```c
uint32_t request_id = event.data.input_requested.request_id;
ESP_ERROR_CHECK(claw_agent_session_respond(
    session_id,
    request_id,
    "approve"));
```

Request ID 只对当前等待点有效。重复、过期或属于其他 Session 的 ID 返回 invalid state。

## Thread 与 ISR

- ABI 内部保护全局 Runtime 状态，但应用仍应让 init/start/stop/deinit 有单一 lifecycle owner。
- 一个打开的 Session stream 只能有一个 consumer；不要让两个 task 并发 `receive` 同一 Session。
- `session_receive` 是唯一带显式 timeout 的阻塞 API；`timeout_ms == 0` 为 non-blocking poll。
- Submit/interrupt/cancel 表示命令被 worker 接受，不表示 Turn 已经终止；继续消费事件确认结果。
- 所有 API 都不适合 ISR：可能锁、分配、调度或访问持久化。

## 错误码

| 错误 | 常见原因 |
| --- | --- |
| `ESP_ERR_INVALID_ARG` | NULL、零 ID、非 UTF-8、未知 enum、部分 API config |
| `ESP_ERR_INVALID_STATE` | Runtime 未启动/重复初始化、stale request、重复 open |
| `ESP_ERR_NOT_FOUND` | Session 不存在或未打开 |
| `ESP_ERR_INVALID_SIZE` | `session_list` buffer 不足 |
| `ESP_ERR_TIMEOUT` | receive 在时间内无 event |
| `ESP_FAIL` | Tool lifecycle、调度、分配或内部错误 |

函数返回值与 event stream error 都要处理。同步返回 `ESP_OK` 只表示命令已接受，异步执行仍可能产生 `ERROR` event。

## `cap_agent` 集成

ESP-Claw 产品固件使用 `cap_agent` 作为 system-only adapter：

- C 系统通过 `method + args` JSON 调用 Session API。
- `cap_agent` 独占每个 Session event stream。
- output delta 聚合到 `OUTPUT_END` 再进入 Event Router。
- route/correlation 与 user Turn FIFO 对齐。

如果使用 `cap_agent`，其他代码不能再直接消费同一个 stream。协议细节见 `components/claw_capabilities/cap_agent/EVENT_ROUTER.md`。

## ABI 兼容

`claw_agent.h`、`claw-cabi` features 和 `.a.zst` 必须同步。修改 struct、enum 或 union 后：

1. 更新 C/Rust 转换与测试。
2. 更新本页和 header comments。
3. 重建四个 target archives。
4. 运行 `./build-idf-staticlib.sh --check`。
5. 构建至少一个固件 target。
