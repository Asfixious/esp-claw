//! Memory scope boundaries for context assembly.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::protocol::TaskId;

/// Runtime role for memory read policy (agent layer TBD).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RuntimeRole {
    Frontend,
    Worker,
}

/// Logical memory partition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MemoryScope {
    FrontendPrivate,
    WorkerPrivate,
    TaskShared,
    ProjectShared,
    GlobalPolicy,
}

/// Scoped key/value store (in-memory first; FATFS later).
pub trait ScopedMemoryStore: Send + Sync {
    fn read(&self, scope: MemoryScope, key: &str) -> Option<String>;
    fn write(&self, scope: MemoryScope, key: &str, value: String);
}

/// Default in-memory implementation.
#[derive(Default)]
pub struct InMemoryScopedStore {
    data: Mutex<HashMap<(MemoryScope, String), String>>,
}

impl InMemoryScopedStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ScopedMemoryStore for InMemoryScopedStore {
    fn read(&self, scope: MemoryScope, key: &str) -> Option<String> {
        self.data
            .lock()
            .unwrap()
            .get(&(scope, key.to_string()))
            .cloned()
    }

    fn write(&self, scope: MemoryScope, key: &str, value: String) {
        self.data
            .lock()
            .unwrap()
            .insert((scope, key.to_string()), value);
    }
}

/// Returns whether `role` may read `scope`.
pub fn role_may_read(role: RuntimeRole, scope: MemoryScope) -> bool {
    match (role, scope) {
        (_, MemoryScope::GlobalPolicy) => true,
        (
            RuntimeRole::Frontend,
            MemoryScope::FrontendPrivate | MemoryScope::TaskShared | MemoryScope::ProjectShared,
        ) => true,
        (
            RuntimeRole::Worker,
            MemoryScope::WorkerPrivate | MemoryScope::TaskShared | MemoryScope::ProjectShared,
        ) => true,
        _ => false,
    }
}

/// Build a memory snapshot string for context injection, enforcing read policy.
pub fn snapshot_for_role(
    store: &dyn ScopedMemoryStore,
    role: RuntimeRole,
    task_id: Option<TaskId>,
) -> String {
    let task_prefix = task_id.map(|id| id.to_wire());
    let mut lines = Vec::new();
    for scope in [
        MemoryScope::GlobalPolicy,
        MemoryScope::TaskShared,
        MemoryScope::ProjectShared,
        MemoryScope::FrontendPrivate,
        MemoryScope::WorkerPrivate,
    ] {
        if !role_may_read(role, scope) {
            continue;
        }
        let prefix = match scope {
            MemoryScope::TaskShared => task_prefix.as_deref().unwrap_or("task"),
            _ => "",
        };
        let key = match scope {
            MemoryScope::TaskShared => format!("{prefix}.shared"),
            MemoryScope::FrontendPrivate => "frontend.notes".to_string(),
            MemoryScope::WorkerPrivate => "worker.notes".to_string(),
            MemoryScope::ProjectShared => "project.context".to_string(),
            MemoryScope::GlobalPolicy => "global.policy".to_string(),
        };
        if let Some(value) = store.read(scope, &key) {
            lines.push(format!("[{scope:?}] {key}={value}"));
        }
    }
    if lines.is_empty() {
        "(empty)".to_string()
    } else {
        lines.join("\n")
    }
}
