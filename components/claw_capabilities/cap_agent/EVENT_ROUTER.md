# cap_agent RPC 与 Event Router 协议

本文定义 C 系统访问 `claw_agent.h` 的唯一边界。该协议是破坏性迁移，
不接受旧版嵌套请求，也不存在 Agent 专用 action。

## 1. 边界与数据流

同步控制面统一调用：

```c
claw_cap_call("agent", input_json, &ctx, output, output_size);
```

`input_json` 固定为：

```json
{
  "method": "session.input",
  "args": {
    "text": "hello"
  }
}
```

异步输出面固定为：

```text
claw_agent.h Session stream
  -> cap_agent event pump
  -> claw_event_t
  -> Event Router
  -> outbound binding
  -> IM send capability
```

在生产 IM/RPC 链路中只有 `cap_agent` 消费 Session stream。Event Router 不调用
`claw_agent.h`，也不保存 Session 状态；IM capability 不消费 Agent stream。
设备诊断 CLI 仅消费自己创建的 ephemeral Session，不与 `cap_agent` 共享 stream。

## 2. RPC envelope

请求必须且只能包含 `method` 和 `args`：

```json
{
  "method": "<method>",
  "args": {}
}
```

通用成功响应：

```json
{
  "ok": true,
  "method": "<method>",
  "result": {}
}
```

通用错误响应：

```json
{
  "ok": false,
  "method": "<method>",
  "error": {
    "code": 258,
    "name": "ESP_ERR_INVALID_ARG",
    "message": "ESP_ERR_INVALID_ARG"
  }
}
```

错误同时通过 `claw_cap_call` 的 `esp_err_t` 返回；调用方不能只检查 JSON。

### ID 解析

- `args.session_id` 优先；缺省时读取 `ctx.session_id` 的十进制正整数。
- `args.request_id` 优先；缺省时读取非零 `ctx.request_id`。
- Event Router 会将 `claw_event_t.session_id` 和 `request_id` 写入 call context。
- ID 必须在 `1..UINT32_MAX`，不接受浮点数、负数和字符串形式的 JSON 参数。

### Route 解析

`session.submit` 和 submit 模式的 `session.input` 将回复路由绑定到本次输入：

- channel：`ctx.target_channel`，缺省为 `ctx.channel`
- chat：`ctx.target_chat_id`，缺省为 `ctx.chat_id`
- correlation：`ctx.correlation_id`

路由按成功提交的 FIFO 顺序与后续 user turn 对齐。一个 Session 可以连续排队
多个输入，不使用“最后一次路由覆盖前一次”的单槽状态。

## 3. RPC methods

### `session.create`

```json
{"method":"session.create","args":{"persistence":"persistent"}}
```

`persistence` 为 `persistent` 或 `ephemeral`。

```json
{"ok":true,"method":"session.create","result":{"session_id":12}}
```

### `session.open`

```json
{"method":"session.open","args":{"session_id":12}}
```

打开 Session，并由 `cap_agent` 独占附加一个长期 event pump。

```json
{"ok":true,"method":"session.open","result":{"session_id":12,"attached":true}}
```

### `session.list`

```json
{"method":"session.list","args":{}}
```

```json
{"ok":true,"method":"session.list","result":{"sessions":[12,18]}}
```

### `session.submit`

```json
{"method":"session.submit","args":{"session_id":12,"text":"hello"}}
```

只接受普通用户输入，不接受 `request_id`。

### `session.respond`

```json
{
  "method": "session.respond",
  "args": {
    "session_id": 12,
    "request_id": 7,
    "text": "approve"
  }
}
```

`request_id` 必填，用于回答 `INPUT_REQUESTED`。

### `session.input`

```json
{"method":"session.input","args":{"text":"hello"}}
```

这是 IM/Event Router 的标准入口：

- 有 `request_id` 时调用 `claw_agent_session_respond`。
- 无 `request_id` 时调用 `claw_agent_session_submit`。

因此路由规则不需要区分普通消息与权限回复。

### `session.interrupt` / `session.cancel` / `session.close` / `session.delete`

```json
{"method":"session.interrupt","args":{"session_id":12}}
```

四个方法都接受相同的 `session_id` 参数。`close` 的 `CLOSED` 事件由 event pump
消费后再释放 pump；删除已关闭 Session 使用 `session.delete`。

### `session.command`

```json
{"method":"session.command","args":{"text":"/session list"}}
```

处理 IM 的 `/session` 命令，并返回可直接发送给用户的纯文本。该方法是唯一不使用
通用 JSON 成功 envelope 的方法，目的是让后续 `send_message` 直接使用
`last.output`。

## 4. Event Router 入站规则

所有普通 IM 文本使用通用 `call_cap`：

```json
{
  "type": "call_cap",
  "cap": "agent",
  "input": {
    "method": "session.input",
    "args": {
      "text": "{{event.text}}"
    }
  }
}
```

Event Router 自动从入站 `claw_event_t` 透传 Session、request、route 和
correlation context。规则不得复制另一套 Session 选择逻辑。

`/session` 使用：

```json
{
  "type": "call_cap",
  "cap": "agent",
  "input": {
    "method": "session.command",
    "args": {
      "text": "{{event.text}}"
    }
  }
}
```

## 5. Agent 事件 envelope

`cap_agent` 发布的所有事件至少包含：

| `claw_event_t` 字段 | 约束 |
| --- | --- |
| `source_cap` | 固定为 `agent` |
| `event_id` | `agent-<session_id>-<monotonic_us>-<sequence>` |
| `event_type` | `out_message`、`agent_event` 或 `agent_stage` |
| `content_type` | 见事件映射表 |
| `session_id` | 当前 numeric Session |
| `request_id` | 仅输入请求事件非零 |
| `source_channel` / `target_channel` | 接受该 user turn 时绑定的 channel |
| `chat_id` / `target_endpoint` | 接受该 user turn 时绑定的 chat |
| `correlation_id` | 接受该 user turn 时绑定的 correlation |
| `text` | 用户可见文本；纯结构事件为空 |
| `payload_json` | 含 `kind` 的结构化 JSON |

### Stream 映射

| `claw_agent_event_kind_t` | Router event | 处理 |
| --- | --- | --- |
| `TURN_STARTED` | `agent_event/turn_started` | user turn 从 pending route FIFO 取路由 |
| `INPUT_REQUESTED` | `out_message/input_request` | 文本提示用户，并携带 `request_id` |
| `ITERATION_STARTED` | `agent_event/iteration_started` | 结构事件 |
| `REASONING_DELTA/END` | 不发布 | reasoning 不进入规则队列 |
| `OUTPUT_DELTA` | 不逐条发布 | 在 pump 内聚合 |
| `OUTPUT_END` | `out_message/text` | 发布一条完整消息 |
| `TOOL_CALL` | `agent_stage/tool_call` | 发布工具阶段信息 |
| `TOOL_CALLS_END` | 不发布 | 无额外语义 |
| `ITERATION_ENDED` | 不发布 | 无额外语义 |
| `TURN_ENDED` | `agent_event/turn_ended` | 清理 active route 与 pending request |
| `ERROR` | `out_message/error` | 发布用户可见错误，抑制本 turn 后续输出 |
| `USAGE` | `agent_event/usage` | token 字段写入 payload |
| `CLOSED` | `agent_event/closed` | 释放 Session pump |

Event Router 队列不是 token stream。聚合 `OUTPUT_DELTA` 可避免长回复耗尽规则队列，
同时保持 C ABI 的流式语义供其他非 Router consumer 使用。

## 6. Agent 输出到 IM

标准规则：

```json
{
  "id": "agent_out_message",
  "enabled": true,
  "consume_on_match": true,
  "match": {
    "source_cap": "agent",
    "event_type": "out_message"
  },
  "actions": [
    {
      "type": "send_message",
      "input": {
        "message": "{{event.text}}"
      }
    }
  ]
}
```

`send_message` 在未显式指定 channel/chat 时使用 event 的 target/source route，
再通过 `claw_event_router_register_outbound_binding(channel, cap)` 找到对应 IM
发送 capability。应用同时启用默认 Agent-output fallback，使规则文件缺少该规则时
仍能返回 IM；显式匹配的规则优先。

## 7. 生命周期与所有权

- 应用拥有 `claw_agent_init/start/stop/deinit` 和 API credential 配置。
- `cap_agent` 只暴露 Session RPC，不允许 RPC 重建 AgentSystem。
- 一个已打开 Session 只能有一个 `cap_agent` event pump。
- pump 收到的每个 `claw_agent_event_t` 必须调用 `claw_agent_event_free`。
- Event Router publish 会复制 `text`/`payload_json`；pump 在 publish 返回后释放临时内存。
- `agent` capability 不设置 `CLAW_CAP_FLAG_CALLABLE_BY_LLM`，防止模型递归调用自身。

## 8. 失败语义

- 请求 envelope、字段集合或参数类型错误：`ESP_ERR_INVALID_ARG`。
- method 不存在：`ESP_ERR_NOT_SUPPORTED`。
- Session 未打开或没有 event pump：`ESP_ERR_INVALID_STATE`/`ESP_ERR_NOT_FOUND`。
- event queue 满：publish 失败；若这是输入请求，已登记的 IM `request_id` 会回滚。
- pending route 队列满：submit 在进入 Rust Session 前失败，不产生无路由 turn。
- 未注册 outbound binding：Agent event 保留在 Router 结果中，但 IM 发送失败。

## 9. 固件资源上限

| 参数 | 当前值 | 行为 |
| --- | --- | --- |
| 同时附加的 Session pumps | 32 | 超出时 `session.open` 返回 `ESP_ERR_NO_MEM` |
| 每个 Session 的 pending routes | 32 | 超出时输入不会提交给 Rust Session |
| 每个 pump 的 output buffer | 32 KiB | 超出时丢弃该缓冲并发布用户可见 error |
| event pump task stack | 8192 bytes | 每个已打开 Session 一个 FreeRTOS task |
| Session receive slice | 5000 ms | 仅用于周期性观察 stream 错误/关闭，不结束 pump |
