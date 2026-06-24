# claw-trace tooling

Capture a live `TRACE` log from a device monitor and export it as a Chrome trace
you can open in `chrome://tracing` / Perfetto. The grammar these tools parse is
[`../docs/trace-format.md`](../docs/trace-format.md).

## CLI: `claw-trace-capture`

The only exported command. Run a monitor as a child process, parse its `TRACE`
lines live, and write `trace.json` when it exits (or on Ctrl-C).

```bash
# from claw-log/
uv run claw-trace-capture --cmd "cargo espflash monitor" -o trace.json --tee
```

| Option | Default | Meaning |
|--------|---------|---------|
| `--cmd` | — | Monitor command to run, e.g. `"cargo espflash monitor"` or `"idf.py monitor"`. |
| `-o` / `--output` | `trace.json` | Output Chrome Trace Event JSON path. |
| `--encoding` | `utf-8` | Encoding used to decode the monitor's bytes. |
| `--tee` | off | Also echo each captured line to stderr live (watch the log while capturing). |

Why a child process instead of a raw pipe: Ctrl-C is handled cleanly — the
monitor is terminated and the records collected so far are still exported.
Chrome export is a batch step (span-tree reconstruction needs the whole
capture), so the JSON is written once at the end. Occasional corrupt serial
lines are skipped (and counted) rather than aborting the capture.

## Libraries

Three of the packages are importable libraries; `trace_capture` is the glue tool
above. The first three share `claw-log/pyproject.toml`; `serial_log` is a
standalone, dependency-free project with its own `pyproject.toml`.

| Package | Role |
|---------|------|
| `claw_trace` | Pure parser: `parse_line` / `parse` turn lines into `TraceRecord`s; `build_forest` rebuilds the span tree, timing, and inherited context; `render_tree` dumps it. No extra deps. |
| `chrome_export` | Consumes a `claw_trace` forest and writes Chrome Trace Event JSON (`write_chrome_trace`). Spans → complete events, events → instant events, numeric event fields → counter tracks. Depends on `chrometrace`. |
| `serial_log` | Standalone, dependency-free: `LogStream` runs a subprocess and pushes each complete stdout line to `on_log` callbacks (strips ANSI, configurable encoding). See `serial_log/README.md`. |
| `trace_capture` | Glue only: wires `serial_log` → `claw_trace` → `chrome_export` behind `claw-trace-capture`. |

`claw_trace` and `chrome_export` stay importable and are still runnable via
`python -m claw_trace` / `python -m chrome_export`, but are intentionally not
registered as console scripts.

## Develop / test

```bash
# from claw-log/
uv run pytest
uv run ruff format
```
