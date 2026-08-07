# Event Router RPC examples

These examples cover the complete RPC baseline exposed by `claw-event-router`.

| Example | What it demonstrates |
| --- | --- |
| `typed_rpc` | Fixed-layout Zerocopy RPC, registry group/RPC discovery, lane-backed `RpcFrame<T>` views, and all four unary/streaming combinations through one `RpcClient::call::<M>()` API |
| `nested_and_lifecycle` | Calls made through `RpcContext`, call-chain metadata, direct self-call rejection, registration tokens, and unregister behavior |
| `errors` | Duplicate registration, typed signature validation, and fixed-layout method errors |
| `static_lanes` | Fixed `N x M` lane storage, bounded root waiters, lane backpressure, and lane reuse |

Run one example from the `agent-framework` workspace root:

```console
cargo run -p claw-event-router --example typed_rpc
cargo run -p claw-event-router --example nested_and_lifecycle
cargo run -p claw-event-router --example errors
cargo run -p claw-event-router --example static_lanes
```

Compile every example without running it:

```console
cargo test -p claw-event-router --examples
```

The examples use Tokio's current-thread runtime so task-local `Rc` handlers do
not need to implement `Send`. Tokio is a development-only dependency and is not
linked into normal `claw-event-router` library or firmware builds.

Every registry receives application-lifetime `RpcLaneStorage<N, M, Q>`. `N`
bounds active calls, each lane owns one `M`-byte request frame and one `M`-byte
response frame, and `Q` bounds queued root calls. The examples place this
zero-initialized storage directly in static memory with `ConstStaticCell`, the
same no-heap placement model intended for firmware.

Typed request, response, and method-error structs derive Zerocopy's `TryFromBytes`,
`IntoBytes`, `KnownLayout`, and `Immutable` traits. Handlers and clients receive
`RpcFrame<T>` and call `view()` to borrow `&T` directly from the lane. A retained
frame retains its lane, so examples copy any value that must outlive the frame
and drop the frame before starting work that may need another lane.
