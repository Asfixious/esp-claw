//! The build-time **tool-directory contract** validator.
//!
//! Call [`validate_tools_dir`] from a dependent crate's build script (add
//! `claw-tool` to `[build-dependencies]` with `default-features = false,
//! features = ["bake"]`). It enforces, at compile time, the same on-disk layout
//! the [`tool_metadata!`](crate::tool_metadata) macro reads at runtime, so the
//! two halves of the contract cannot drift:
//!
//! - `resources/tools/<name>/` holds **exactly** `schema.json` + `usage.md`
//!   (a missing or stray entry fails the build),
//! - `schema.json` is a function object with a string `function.name`, and
//! - that `function.name` equals the directory name `<name>`.
//!
//! The runtime side separately checks `handler.name() == function.name` (a
//! `debug_assert!` when a `ToolSet` is built), so the full chain
//! `handler.name() == dir == function.name` is closed.
//!
//! This module depends only on `std` + `serde_json`, so a build-dependency on it
//! does not pull the runtime framework (or `claw-permission`) into the build graph.

use std::error::Error;
use std::fs;
use std::path::Path;

use serde_json::Value;

/// The exact entries every `resources/tools/<name>/` directory must contain —
/// no more, no less.
const TOOL_DIR_ENTRIES: &[&str] = &["schema.json", "usage.md"];

/// Validate every tool directory under `tools_dir` (typically
/// `<CARGO_MANIFEST_DIR>/resources/tools`).
///
/// Emits `cargo:rerun-if-changed` for the directory and each tool's two files, so
/// edits (and additions/removals) re-trigger the build. Returns the number of
/// tool directories validated.
///
/// # Errors
///
/// Fails the build if the tools directory cannot be read, is empty, holds a stray
/// (non-hidden) file, a tool directory has a missing/stray entry, a `schema.json`
/// is not a function object with a string `function.name`, or that name does not
/// match the directory name.
pub fn validate_tools_dir(tools_dir: &Path) -> Result<usize, Box<dyn Error>> {
    // Re-run when a tool directory is added or removed.
    println!("cargo:rerun-if-changed={}", tools_dir.display());

    let mut found = 0usize;
    for entry in fs::read_dir(tools_dir)
        .map_err(|error| format!("reading {}: {error}", tools_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("non-UTF-8 entry name in {}", tools_dir.display()))?
            .to_string();

        // Hidden entries (e.g. `.gitkeep`, `.DS_Store`) are not tools; skip them.
        if name.starts_with('.') {
            continue;
        }
        // Anything else must be a proper tool directory: a stray file here would
        // otherwise be silently ignored, so reject it ("no more, no less").
        if !path.is_dir() {
            return Err(format!(
                "{}: unexpected file '{name}' — only tool directories may live under \
                 resources/tools (each holding exactly {TOOL_DIR_ENTRIES:?})",
                tools_dir.display()
            )
            .into());
        }

        for file in TOOL_DIR_ENTRIES {
            println!("cargo:rerun-if-changed={}", path.join(file).display());
        }
        validate_tool(&path, &name)?;
        found = found.saturating_add(1);
    }

    if found == 0 {
        return Err(format!("no tools found under {}", tools_dir.display()).into());
    }
    Ok(found)
}

/// Validate one tool directory: the fixed two-file layout plus
/// `function.name` == `dir_name`.
fn validate_tool(dir: &Path, dir_name: &str) -> Result<(), Box<dyn Error>> {
    // Exactly schema.json + usage.md — a missing or stray entry fails the build.
    ensure_exact_entries(dir, TOOL_DIR_ENTRIES)?;

    let schema_path = dir.join("schema.json");
    let schema_text = fs::read_to_string(&schema_path)
        .map_err(|error| format!("reading {}: {error}", schema_path.display()))?;
    let schema: Value = serde_json::from_str(&schema_text)
        .map_err(|error| format!("parsing {}: {error}", schema_path.display()))?;

    let function_name = schema
        .get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!(
                "{}: schema is not a function object with a string `function.name`",
                schema_path.display()
            )
        })?;

    if function_name != dir_name {
        return Err(format!(
            "tool name mismatch in {}: directory is '{dir_name}' but schema declares \
             function.name '{function_name}'",
            dir.display()
        )
        .into());
    }

    Ok(())
}

/// Enforce that `dir` contains **exactly** `expected` — no missing entry and no
/// stray one. Hidden entries (names starting with `.`) are ignored so VCS/OS
/// artifacts do not fail the build.
fn ensure_exact_entries(dir: &Path, expected: &[&str]) -> Result<(), Box<dyn Error>> {
    let mut actual: Vec<String> = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| format!("reading {}: {error}", dir.display()))? {
        let entry = entry?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| format!("non-UTF-8 entry name in {}", dir.display()))?
            .to_string();
        if name.starts_with('.') {
            continue;
        }
        actual.push(name);
    }

    for want in expected {
        if !actual.iter().any(|name| name == want) {
            return Err(format!(
                "{}: missing required entry '{want}' (a tool directory is fixed: \
                 exactly {expected:?})",
                dir.display()
            )
            .into());
        }
    }
    for got in &actual {
        if !expected.iter().any(|want| want == got) {
            return Err(format!(
                "{}: unexpected entry '{got}' (a tool directory is fixed: \
                 exactly {expected:?}; remove it)",
                dir.display()
            )
            .into());
        }
    }
    Ok(())
}
