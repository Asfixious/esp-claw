//! Run and iteration tracing.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Named span within one iteration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpanName {
    BuildContext,
    LlmHttp,
    RunTool,
    Finalize,
}

/// One iteration trace record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IterationTrace {
    pub run_id: String,
    pub instance_id: String,
    pub iteration_id: u32,
    pub context_hash: String,
    pub action_summary: Option<String>,
    pub observation: Option<String>,
    pub spans: Vec<SpanName>,
}

/// Aggregated trace for one run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RunTrace {
    pub run_id: String,
    pub iterations: Vec<IterationTrace>,
}

/// Sink for traces (in-memory for tests).
pub trait TraceSink: Send + Sync {
    fn record_iteration(&self, trace: IterationTrace);
    fn run_trace(&self, run_id: &str) -> Option<RunTrace>;
}

#[derive(Default)]
pub struct InMemoryTraceSink {
    runs: Mutex<std::collections::HashMap<String, RunTrace>>,
}

impl InMemoryTraceSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn all_iterations(&self, run_id: &str) -> Vec<IterationTrace> {
        self.runs
            .lock()
            .unwrap()
            .get(run_id)
            .map(|r| r.iterations.clone())
            .unwrap_or_default()
    }
}

impl TraceSink for InMemoryTraceSink {
    fn record_iteration(&self, trace: IterationTrace) {
        let mut runs = self.runs.lock().unwrap();
        let entry = runs
            .entry(trace.run_id.clone())
            .or_insert_with(|| RunTrace {
                run_id: trace.run_id.clone(),
                iterations: Vec::new(),
            });
        entry.iterations.push(trace);
    }

    fn run_trace(&self, run_id: &str) -> Option<RunTrace> {
        self.runs.lock().unwrap().get(run_id).cloned()
    }
}

pub fn hash_context(system_prompt: &str, messages_json: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    system_prompt.hash(&mut hasher);
    messages_json.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}
