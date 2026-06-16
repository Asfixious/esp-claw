//! The `ClawFs` filesystem injection trait.
//!
//! This is the persistence seam for everything that has to survive a reboot
//! (conversation tapes, profile/long-term memory, …). Like [`ClawHttp`], it is a
//! dependency-injection point: the espidf wiring implements it over the DATA
//! root (FATFS / SD card), while host tests provide `std::fs` or an in-memory
//! map. Modules never touch `std::fs` directly so they stay portable.
//!
//! Paths are byte-oriented, opaque strings already resolved against the DATA
//! root by the caller (`claw_paths`); this trait does no path joining.
//!
//! [`ClawHttp`]: crate::http::ClawHttp

/// Filesystem failure.
///
/// Deliberately coarse: callers either retry, log, or fall back to an empty
/// state, so the only distinction that matters is "the file isn't there" versus
/// "the underlying I/O failed". The `esp_err_t` mapping for the C ABI lives in
/// `claw_capi`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FsError {
    #[error("path not found")]
    NotFound,
    #[error("filesystem io error: {0}")]
    Io(String),
}

/// Byte-oriented persistence injection point.
///
/// Implementations must be safe to share across threads: a single `ClawFs` is
/// handed (via `Arc`) to the memory task pool, whose worker threads call
/// [`write_atomic`](ClawFs::write_atomic) / [`append`](ClawFs::append) off the
/// foreground path.
///
/// Two write disciplines coexist:
/// - [`append`](ClawFs::append) + [`read_at`](ClawFs::read_at) for append-only
///   journals (e.g. the conversation data `.jsonl`), where each turn appends a
///   record and load reads back only the live records by byte offset.
/// - [`write_atomic`](ClawFs::write_atomic) for whole-file checkpoints that must
///   replace the target tear-free: the small index manifest (`.json`) rewritten
///   on compaction/collapse, and the `.jsonl` itself when a collapse rewrites it
///   to drop dead records.
pub trait ClawFs: Send + Sync {
    /// Read the whole file. Returns [`FsError::NotFound`] when `path` is absent.
    fn read(&self, path: &str) -> Result<Vec<u8>, FsError>;

    /// Read `len` bytes starting at byte `offset`.
    ///
    /// Used to fetch a single record out of an append-only log via its indexed
    /// `(offset, len)` without reading the whole file. Returns
    /// [`FsError::NotFound`] when `path` is absent; an `offset`/`len` past the end
    /// of the file is an [`FsError::Io`] (a corrupt/short file), not a silent
    /// truncation. Implementations should return exactly `len` bytes on success.
    fn read_at(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, FsError>;

    /// Byte length of `path`. Returns [`FsError::NotFound`] when absent.
    ///
    /// Used to seed the append cursor and to detect a log whose tail was written
    /// but whose index entry was not (a crash between the two appends).
    fn len(&self, path: &str) -> Result<u64, FsError>;

    /// Durably replace `path` with `data`.
    ///
    /// Implementations should write to a temporary sibling and `rename` over the
    /// target so a crash mid-write never leaves a half-written file — the file is
    /// either the old contents or the new contents, never a torn mix.
    fn write_atomic(&self, path: &str, data: &[u8]) -> Result<(), FsError>;

    /// Append `data` to the end of `path`, creating it if absent.
    ///
    /// This is the steady-state write for journals: the bytes go after whatever
    /// is already there, so prior records are never rewritten. A crash mid-append
    /// may leave a torn trailing record, which readers discard; it must never
    /// corrupt earlier records. Callers serialize their own appends, so offsets
    /// they computed match the on-disk order.
    fn append(&self, path: &str, data: &[u8]) -> Result<(), FsError>;

    /// Whether `path` currently exists.
    fn exists(&self, path: &str) -> bool;

    /// Remove `path`. Removing a missing path succeeds (idempotent).
    fn remove(&self, path: &str) -> Result<(), FsError>;

    /// List the immediate entry names within directory `path`.
    ///
    /// Returns only the final path component of each entry (e.g. `"light_switch"`),
    /// not joined paths, and in unspecified order — callers that need ordering
    /// sort themselves. Both files and subdirectories are included. Returns
    /// [`FsError::NotFound`] when `path` does not exist. Used to enumerate the
    /// skills root (one directory per skill) without reading any file.
    fn list_dir(&self, path: &str) -> Result<Vec<String>, FsError>;
}
