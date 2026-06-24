# claw-trace

A small Python library that parses claw-log's flat-tree `TRACE` log format and
reconstructs the span tree, per-span timing, and the inherited context that each
line implies. The authoritative grammar is
[`../docs/trace-format.md`](../docs/trace-format.md).

The uv project lives in `claw-log/`; this library is the `claw_trace` package
under `claw-log/scripts/`.

## Use as a library

```python
from claw_trace import build_forest, render_tree

with open("device.log", encoding="utf-8") as handle:
    forest = build_forest(handle)

print(render_tree(forest))

# Or inspect programmatically:
for span in forest.spans.values():
    print(span.name, span.duration_ms, span.context)
```

- `parse(source)` / `parse_line(line)` — turn lines into `TraceRecord`s. Lines
  with no `TRACE` marker are skipped (`parse_line` returns `None`); a malformed
  record raises `ParseError`.

### Per-line adapters

Every parse entry point (`parse`, `parse_line`, `build_forest`) takes an optional
`adapter`: `Callable[[str], str | None]` run on each raw line before the `TRACE`
marker is located. It can rewrite a line (strip a prefix, drop ANSI colors) or
return `None` to skip it. `TraceRecord.raw` keeps the original, un-adapted line.

```python
from claw_trace import build_forest, strip_ansi, keep_after_marker, chain

# Strip ANSI colors emitted on a TTY, then keep only the TRACE payload.
forest = build_forest(text, adapter=chain(strip_ansi, keep_after_marker()))

# Custom adapter: drop warning lines entirely.
forest = build_forest(text, adapter=lambda l: None if l.startswith("W ") else l)
```

Built-ins: `strip_ansi`, `keep_after_marker(marker="TRACE")`, `chain(*adapters)`.
The default (no adapter) already handles the common
`I (2153) claw_core::iteration_loop: TRACE ...` transport prefix, since the
parser searches for the marker instead of anchoring at column 0.
- `build_forest(source)` — replay `enter`/`exit` per task to rebuild the
  `(span, parent)` tree, resolve effective context (child keys shadow ancestors),
  and anchor events to their spans.
- `render_tree(forest)` — indented, human-readable dump.

The library is intentionally pure (parse + reconstruct only). Format exporters
that pull in extra dependencies — such as the Chrome Trace exporter below — live
*outside* the `claw_trace` package as separate tools that consume it.

## CLI

From the `claw-log/` directory:

```bash
uv run claw-trace device.log
# or
cat device.log | uv run claw-trace
# strip ANSI colors first (e.g. captured TTY output):
uv run claw-trace --strip-ansi device.log
```

## Chrome Trace export (separate tool)

The `chrome_export` package (under `scripts/`, **separate from the `claw_trace`
library**) consumes a forest and writes the Chrome Trace Event Format, loadable
in `chrome://tracing` / Perfetto. Spans become *complete* events, trace events
become *instant* events, and numeric `key=value` fields on an event also become
*counter* tracks (e.g. `free_heap` for RAM). `session` maps to a process, `task`
to a thread. It depends on `chrometrace`; the `claw_trace` lib does not.

```bash
uv run claw-trace-chrome device.log -o trace.json
cat device.log | uv run claw-trace-chrome --strip-ansi -o trace.json
```

```python
from claw_trace import build_forest
from chrome_export import write_chrome_trace

write_chrome_trace(build_forest(open("device.log")), "trace.json")
```

## Develop / test

From `claw-log/`:

```bash
uv run pytest
```
