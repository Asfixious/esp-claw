//! Constants mirroring `claw_memory_internal.h`.

pub const DEFAULT_MAX_MESSAGE_CHARS: usize = 4096;
pub const DEFAULT_MAX_TOOL_ITERATIONS: u32 = 10;
pub const MAX_PATH: usize = 192;
pub const MAX_SUMMARIES: usize = 3;
pub const MAX_LABEL_CHARS: usize = 8;
pub const MAX_LABEL_TEXT: usize = 40;
pub const MAX_ACTIVE_ITEMS: usize = 128;
pub const COMPACT_CHANGE_THRESHOLD: u32 = 5;
pub const COMPACT_SIZE_THRESHOLD: usize = 32 * 1024;
pub const SESSION_SIZE_LIMIT: usize = 150 * 1024;
pub const RECALL_DEFAULT_LIMIT: usize = 8;
pub const RECORDS_FILE: &str = "memory_records.jsonl";
pub const INDEX_FILE: &str = "memory_index.json";
pub const DIGEST_FILE: &str = "memory_digest.log";
pub const MARKDOWN_FILE: &str = "MEMORY.md";
pub const SOUL_FILE: &str = "soul.md";
pub const IDENTITY_FILE: &str = "identity.md";
pub const USER_FILE: &str = "user.md";
pub const AUTO_EXTRACT_MAX_ITEMS: usize = 3;

// claw_memory_item_t field capacities (sizeof of each char array).
pub const ID_CAP: usize = 40;
pub const SOURCE_CAP: usize = 16;
pub const CONTENT_CAP: usize = 256;
pub const TAGS_CAP: usize = 96;
pub const KEYWORDS_CAP: usize = 128;

// Session history binary index.
pub const SESSION_IDX_MAGIC: u32 = 0x5844_4843; // CHDX
pub const SESSION_IDX_VERSION: u16 = 1;
pub const SESSION_IDX_HEADER_SIZE: usize = 8;
pub const SESSION_IDX_ENTRY_SIZE: usize = 10;
pub const SESSION_COMPACT_TOOL_TURNS: usize = 1;

pub const SESSION_SIZE_WARNING: &str = "Session history is still too large after compaction. Please create a new conversation by sending     the command `/session new [name]`, and delete the old session by `/session delete <name>` due to limited storage space.";

// claw_core context kinds / record types (mirror claw_core.h).
pub const CONTEXT_KIND_SYSTEM_PROMPT: i32 = 0;
pub const CONTEXT_KIND_MESSAGES: i32 = 1;

pub const CONTEXT_RECORD_USER: i32 = 1;
pub const CONTEXT_RECORD_ASSISTANT_FINAL: i32 = 2;
pub const CONTEXT_RECORD_ASSISTANT_TOOL: i32 = 3;
pub const CONTEXT_RECORD_TOOL_RESULT: i32 = 4;

pub const CONTEXT_PROVIDER_FLAG_REQUEST_START_ONLY: u32 = 1 << 0;

/// `claw_memory_backend_format_t`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendFormat {
    Unknown = 0,
    OpenAi = 1,
    Anthropic = 2,
}

impl BackendFormat {
    pub fn from_type(backend_type: Option<&str>) -> BackendFormat {
        match backend_type {
            Some("openai_compatible") => BackendFormat::OpenAi,
            Some("anthropic_compatible") => BackendFormat::Anthropic,
            _ => BackendFormat::Unknown,
        }
    }

    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn from_u8(v: u8) -> Option<BackendFormat> {
        match v {
            0 => Some(BackendFormat::Unknown),
            1 => Some(BackendFormat::OpenAi),
            2 => Some(BackendFormat::Anthropic),
            _ => None,
        }
    }
}

/// `claw_memory_message_intent_t`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageIntent {
    None,
    Forget,
    Replace,
}

impl MessageIntent {
    #[cfg_attr(not(target_os = "espidf"), allow(dead_code))]
    pub fn from_str(value: Option<&str>) -> MessageIntent {
        match value {
            Some("forget") => MessageIntent::Forget,
            Some("replace") => MessageIntent::Replace,
            _ => MessageIntent::None,
        }
    }
}
