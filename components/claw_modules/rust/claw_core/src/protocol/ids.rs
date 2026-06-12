//! Strongly typed identifiers.
//!
//! In memory these are numeric (`usize`). On the wire (JSON and [`Display`]) they use
//! prefixed strings: `session-1`, `task-1`, `step-1`, `turn-1`, `worker-1`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

macro_rules! define_prefixed_id {
    ($name:ident, $prefix:literal, $kind:literal) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name(pub usize);

        impl $name {
            pub const fn new(id: usize) -> Self {
                Self(id)
            }

            pub fn to_wire(&self) -> String {
                format!(concat!($prefix, "{}"), self.0)
            }

            pub fn from_wire(value: &str) -> Result<Self, IdParseError> {
                parse_prefixed_id(value, $prefix, $kind).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::from_wire(value)
            }
        }

        impl From<usize> for $name {
            fn from(value: usize) -> Self {
                Self(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.to_wire())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                Self::from_wire(&value).map_err(serde::de::Error::custom)
            }
        }
    };
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum IdParseError {
    #[error("empty id string")]
    Empty,
    #[error("invalid {kind} id: {value}")]
    Invalid { kind: &'static str, value: String },
}

define_prefixed_id!(SessionId, "session-", "session");
define_prefixed_id!(TaskId, "task-", "task");
define_prefixed_id!(StepId, "step-", "step");
define_prefixed_id!(TurnId, "turn-", "turn");
define_prefixed_id!(WorkerId, "worker-", "worker");

impl TurnId {
    /// C / capability surface (`claw_cap_call_context_t.request_id`).
    pub fn as_request_id(self) -> u32 {
        self.0 as u32
    }
}

impl WorkerId {
    /// Default mapping: one worker instance per task, same numeric suffix.
    pub fn for_task(task_id: TaskId) -> Self {
        Self(task_id.0)
    }
}

fn parse_prefixed_id(value: &str, prefix: &str, kind: &'static str) -> Result<usize, IdParseError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(IdParseError::Empty);
    }

    let rest = trimmed.strip_prefix(prefix).ok_or_else(|| IdParseError::Invalid {
        kind,
        value: value.to_string(),
    })?;

    rest.parse::<usize>().map_err(|_| IdParseError::Invalid {
        kind,
        value: value.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn session_id_serializes_to_prefixed_string() {
        let value = serde_json::to_value(SessionId(1)).unwrap();
        assert_eq!(value, json!("session-1"));
    }

    #[test]
    fn task_id_serializes_to_prefixed_string() {
        let value = serde_json::to_value(TaskId(42)).unwrap();
        assert_eq!(value, json!("task-42"));
    }

    #[test]
    fn step_id_serializes_to_prefixed_string() {
        let value = serde_json::to_value(StepId(3)).unwrap();
        assert_eq!(value, json!("step-3"));
    }

    #[test]
    fn turn_id_serializes_to_prefixed_string() {
        let value = serde_json::to_value(TurnId(5)).unwrap();
        assert_eq!(value, json!("turn-5"));
    }

    #[test]
    fn worker_id_serializes_to_prefixed_string() {
        let value = serde_json::to_value(WorkerId(5)).unwrap();
        assert_eq!(value, json!("worker-5"));
    }

    #[test]
    fn ids_deserialize_from_prefixed_string() {
        let session: SessionId = serde_json::from_value(json!("session-7")).unwrap();
        let task: TaskId = serde_json::from_value(json!("task-3")).unwrap();
        let step: StepId = serde_json::from_value(json!("step-2")).unwrap();
        let turn: TurnId = serde_json::from_value(json!("turn-4")).unwrap();
        let worker: WorkerId = serde_json::from_value(json!("worker-9")).unwrap();
        assert_eq!(session, SessionId(7));
        assert_eq!(turn, TurnId(4));
        assert_eq!(task, TaskId(3));
        assert_eq!(step, StepId(2));
        assert_eq!(worker, WorkerId(9));
        assert_eq!(WorkerId::for_task(TaskId(3)), WorkerId(3));
    }

    #[test]
    fn ids_reject_non_prefixed_wire_values() {
        assert!(serde_json::from_value::<SessionId>(json!("sess-7")).is_err());
        assert!(serde_json::from_value::<SessionId>(json!(7)).is_err());
        assert!(serde_json::from_value::<TaskId>(json!(3)).is_err());
        assert!(serde_json::from_value::<StepId>(json!("S1")).is_err());
        assert!(SessionId::from_wire("7").is_err());
        assert!(StepId::from_wire("step-").is_err());
    }

    #[test]
    fn command_roundtrip_uses_wire_ids() {
        use crate::protocol::Command;

        let command = Command::CreateTask {
            task_id: TaskId(1),
            goal: "build".into(),
            frontend_instance_id: "fe-1".into(),
            requires_plan_approval: false,
        };
        let value = serde_json::to_value(&command).unwrap();
        assert_eq!(value["CreateTask"]["task_id"], json!("task-1"));

        let restored: Command = serde_json::from_value(value).unwrap();
        assert_eq!(
            restored,
            Command::CreateTask {
                task_id: TaskId(1),
                goal: "build".into(),
                frontend_instance_id: "fe-1".into(),
                requires_plan_approval: false,
            }
        );
    }

    #[test]
    fn display_matches_wire_format() {
        assert_eq!(SessionId(1).to_string(), "session-1");
        assert_eq!(TaskId(1).to_string(), "task-1");
        assert_eq!(StepId(1).to_string(), "step-1");
        assert_eq!(TurnId(1).to_string(), "turn-1");
        assert_eq!(WorkerId(1).to_string(), "worker-1");
        assert_eq!(TurnId(42).as_request_id(), 42);
    }
}
