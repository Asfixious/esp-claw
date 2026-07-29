---
{
  "name": "cap_im_platform",
  "description": "How to send additional text, images, or local files back to IM chats.",
  "metadata": {
    "cap_groups": [
      "cap_im_gateway"
    ],
    "manage_mode": "readonly"
  }
}
---

# IM Messaging

Use this skill only when the user asks to send an extra text message, image, or local file through an IM channel. Normal agent replies are sent automatically and do not need IM send tools.

## Channel Selection

Prefer the current request `source_cap`. Do not send through another IM channel unless the user explicitly asks. Do not invent a target chat when the current context or user request does not provide one.

| Channel | `channel` | Text | Image | File | Chat target |
| --- | --- | --- | --- | --- | --- |
| Feishu | `feishu` | `send_message` | `send_image` | `send_file` | Explicit targets may be Feishu `chat_id` values, or user `open_id` values beginning with `ou_`. |
| QQ | `qq` | `send_message` | `send_image` | `send_file` | Explicit targets are normalized as `c2c:<openid>` or `group:<group_openid>`. |
| Telegram | `telegram` | `send_message` | `send_image` | `send_file` | Explicit targets are numeric strings such as `123456789` or `-1001234567890`. |
| WeChat | `wechat` | `send_message` | `send_image` | Not supported | Preserve concrete room or contact ids exactly. |

## Chat Targets

Calls are routed by `channel` plus `chat_id`. They may come from the current call context; pass both explicitly when starting a new outbound send or when the target would otherwise be ambiguous.

## Rules

- Call the unified IM Gateway capability: `send_message` for text, `send_image` for image files, and `send_file` for local non-image files.
- For WeChat, use `send_message` or `send_image`; generic non-image file send is not available.
- Use image tools for `.jpg`, `.jpeg`, `.png`, `.gif`, or `.webp`. Use file tools for non-image files such as `.txt`, `.json`, `.log`, `.csv`, `.pdf`, or archives.
- `path` must be a real local device path. If unknown, inspect storage first with file tools such as `list_dir`.
- Do not pass remote URLs directly to IM send tools. Download or locate the file on local storage first.
- In this demo app, inbound IM attachments are typically saved under `<storage_root>/inbox`.
- `caption` is optional for image and file sends. For Feishu media sends, caption is delivered as a follow-up text message.
- Feishu text is sent through a Markdown-capable interactive card when possible, with fallback to plain text if card construction or delivery fails.
- If the send tool returns an error, report the error directly. Do not retry or switch channels unless the user asks.
- If a capability returns success text or JSON such as `{"ok":true}`, tell the user the message or file has already been sent; do not phrase it as pending.
- QQ generic file delivery may still depend on QQ platform-side support. If `send_file` fails, report the failure clearly and only consider image send when the file is actually an image.

## Workflow

1. Confirm the target `channel` from the current context or the user's explicit request.
2. Determine whether the user wants text, an image, or a generic file.
3. Resolve the target chat according to the channel rules above.
4. Resolve the local file path when sending media.
5. Call the matching unified Gateway capability.
6. After success, tell the user the message, image, or file has already been sent.

## Examples

Reply with text when the current call context already contains the route:
```json
{
  "message": "The task has been completed."
}
```

Send text to an explicit Feishu user:
```json
{
  "channel": "feishu",
  "chat_id": "ou_xxx123456",
  "message": "Latest status: device is online."
}
```

Send an image to an explicit Telegram chat:
```json
{
  "channel": "telegram",
  "chat_id": "-1001234567890",
  "path": "<storage_root>/inbox/capture.jpg",
  "caption": "Here is the image."
}
```

Send a file to an explicit Feishu chat:
```json
{
  "channel": "feishu",
  "chat_id": "oc_xxx123456",
  "path": "<storage_root>/reports/status.json",
  "caption": "Latest report."
}
```

Send text to a WeChat chat:
```json
{
  "channel": "wechat",
  "chat_id": "room123",
  "message": "Latest status: device is online."
}
```
