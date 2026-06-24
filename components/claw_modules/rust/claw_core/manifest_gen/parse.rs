//! Read and validate one `resources/agents/<kind>/` directory into a
//! [`ParsedManifest`]. Any malformed JSON, missing file, or kind/dir mismatch is
//! returned as an error so the build script can fail the build with a clear
//! message.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use crate::model::{AgentJson, CapabilitiesJson, SkillsJson};

/// A fully-parsed, validated manifest — the build-time counterpart to the
/// runtime `AgentManifest`. Strings are owned here; [`crate::codegen`] renders
/// them into `&'static` data.
pub struct ParsedManifest {
    pub kind: String,
    pub description: String,
    pub spawn_enabled: bool,
    pub allowed_kinds: Vec<String>,
    pub retries: u32,
    pub tool_block_retries: Option<u32>,
    pub capabilities: Vec<String>,
    pub skills: Vec<String>,
    /// Absolute path to `instructions.md`, embedded via `include_str!` in the
    /// generated code so the bytes are not duplicated into the generated source.
    pub instructions_path: PathBuf,
}

/// The manifest files expected in every kind directory; also the set the build
/// script registers for `rerun-if-changed`.
pub const MANIFEST_FILES: &[&str] = &[
    "agent.json",
    "capabilities/capabilities.json",
    "skills/skills.json",
    "instructions.md",
];

/// Files the shared `common/` base is tracked for `rerun-if-changed`. `agent.json`
/// is included so that *adding* one re-triggers the build (and fails it, since
/// the shared base must not declare an agent kind).
pub const COMMON_FILES: &[&str] = &[
    "capabilities/capabilities.json",
    "skills/skills.json",
    "agent.json",
];

/// Capability and skill names contributed by the shared `common/` base, inherited
/// by every kind.
pub struct CommonBase {
    pub capabilities: Vec<String>,
    pub skills: Vec<String>,
}

/// Parse the shared `common/` base at `common_dir`.
///
/// `common/` carries the default `capabilities/` and `skills/` inherited by every
/// kind. Both files are required (like a kind's), but it must **not** declare an
/// agent kind: an `agent.json` there is an error.
///
/// # Errors
///
/// Errors if `common/` contains `agent.json`, or if its `capabilities.json` /
/// `skills.json` is missing or malformed.
pub fn parse_common(common_dir: &Path) -> Result<CommonBase, Box<dyn Error>> {
    if common_dir.join("agent.json").is_file() {
        return Err(format!(
            "{} must not contain agent.json: the shared base defines default \
             capabilities/skills inherited by all kinds, not an agent kind",
            common_dir.display()
        )
        .into());
    }

    let capabilities: CapabilitiesJson = read_json(common_dir, "capabilities/capabilities.json")?;
    let skills: SkillsJson = read_json(common_dir, "skills/skills.json")?;

    Ok(CommonBase {
        capabilities: capabilities.capabilities,
        skills: skills.skills,
    })
}

/// Parse and validate the kind directory at `dir`.
///
/// The directory name is the source of truth for the kind: `agent.json`'s
/// declared `kind` must match it, otherwise the build fails.
pub fn parse_kind(dir: &Path) -> Result<ParsedManifest, Box<dyn Error>> {
    let dir_name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("agent kind directory has no UTF-8 name: {}", dir.display()))?
        .to_string();

    let agent: AgentJson = read_json(dir, "agent.json")?;
    if agent.kind != dir_name {
        return Err(format!(
            "kind mismatch in {}: directory is '{dir_name}' but agent.json declares '{}'",
            dir.display(),
            agent.kind
        )
        .into());
    }

    let capabilities: CapabilitiesJson = read_json(dir, "capabilities/capabilities.json")?;
    let skills: SkillsJson = read_json(dir, "skills/skills.json")?;

    let instructions_path = dir.join("instructions.md");
    if !instructions_path.is_file() {
        return Err(format!("missing instructions.md in {}", dir.display()).into());
    }

    Ok(ParsedManifest {
        kind: agent.kind,
        description: agent.description,
        spawn_enabled: agent.spawn.enabled,
        allowed_kinds: agent.spawn.allowed_kinds,
        retries: agent.runtime.retries,
        tool_block_retries: agent.runtime.tool_block_retries,
        capabilities: capabilities.capabilities,
        skills: skills.skills,
        instructions_path,
    })
}

/// Read and deserialize `dir/<relative>` as JSON, wrapping IO/parse errors with
/// the offending path.
fn read_json<T: serde::de::DeserializeOwned>(
    dir: &Path,
    relative: &str,
) -> Result<T, Box<dyn Error>> {
    let path = dir.join(relative);
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("reading {}: {error}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("parsing {}: {error}", path.display()).into())
}
