# Event Router RPC examples

These examples cover the complete RPC baseline exposed by `claw-event-router`.

| Example | What it demonstrates |
| --- | --- |
| `typed_rpc` | Struct-based RPC and all four unary/streaming combinations through one `RpcClient::call::<M>()` API |
| `raw_binary` | The `BinaryReader -> RawRpcProvider -> BinaryWriter` escape hatch, bounded pipes, backpressure, and explicit concurrent driving |
| `nested_and_lifecycle` | Calls made through `RpcContext`, call-chain metadata, direct self-call rejection, registration tokens, and unregister behavior |
| `errors_and_limits` | Duplicate registration, typed signature validation, frame limits, invalid method configuration, and structured provider failures |

Run one example from the `agent-framework` workspace root:

```console
cargo run -p claw-event-router --example typed_rpc
cargo run -p claw-event-router --example raw_binary
cargo run -p claw-event-router --example nested_and_lifecycle
cargo run -p claw-event-router --example errors_and_limits
```

Compile every example without running it:

```console
cargo test -p claw-event-router --examples
```

Typed RPC is the normal Rust component API. Raw RPC is intentionally lower
level and remains useful for Lua, C ABI, network adapters, and custom codecs.
The examples use Tokio's current-thread runtime so task-local `Rc` providers do
not need to implement `Send`. Tokio is a development-only dependency and is not
linked into normal `claw-event-router` library or firmware builds.
