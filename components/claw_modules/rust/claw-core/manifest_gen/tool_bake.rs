//! The **tool-schema** baker/validator.
//!
//! Validates `resources/tools/<function.name>/` so the bake layout is enforced at
//! compile time (fail the build, not the device). Each tool directory must hold
//! **exactly** two files:
//! - `schema.json` — the OpenAI function schema sent to the LLM, and
//! - `usage.md` — the soft-tools prompt prose (may be blank ⇒ no usage).
//!
//! and the directory name must equal the schema's `function.name`. The text
//! itself is pulled in by the `tool_metadata!` macro in `claw_core` via
//! `include_str!`; this step only enforces the contract and registers the files
//! for `rerun-if-changed`. The runtime side separately checks
//! `handler.name() == function.name`, so the full chain
//! `handler.name() == dir == function.name` is closed.

use std::error::Error;
use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::parse::ensure_exact_entries;

/// The exact entries every `resources/tools/<name>/` directory must contain —
/// no more, no less.
const TOOL_DIR_ENTRIES: &[&str] = &["schema.json", "usage.md"];

/// Validate every tool directory under `<manifest_dir>/resources/tools`.
///
/// Registers the tools directory and each tool's two files for
/// `rerun-if-changed` so edits (and additions/removals) re-trigger the build.
///
/// # Errors
///
/// Fails the build if the tools directory cannot be read, a tool directory has a
/// missing/stray entry, a `schema.json` is not a function object with a string
/// `function.name`, or that name does not match the directory name.
pub fn generate(manifest_dir: &Path) -> Result<(), Box<dyn Error>> {
    let tools_dir = manifest_dir.join("resources/tools");
    // Re-run when a tool directory is added or removed.
    println!("cargo:rerun-if-changed={}", tools_dir.display());

    let mut found = 0usize;
    for entry in
        fs::read_dir(&tools_dir).map_err(|error| format!("reading {}: {error}", tools_dir.display()))?
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
        found += 1;
    }

    if found == 0 {
        return Err(format!("no tools found under {}", tools_dir.display()).into());
    }
    Ok(())
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
