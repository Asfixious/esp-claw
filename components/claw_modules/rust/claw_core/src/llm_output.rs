//! Validated structured LLM outputs per phase schema.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::protocol::StepId;

/// Parsed and schema-validated model output for one semantic phase.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidatedLlmOutput {
    FrontendIntake {
        action: FrontendIntakeAction,
        message: Option<String>,
    },
    FrontendReport {
        summary: String,
    },
    WorkerPlan {
        steps: Vec<StepId>,
    },
    WorkerVerify {
        passed: bool,
        summary: Option<String>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FrontendIntakeAction {
    TaskUnderstood,
    NeedClarification,
}

#[derive(Error, Debug, PartialEq, Eq)]
pub enum LlmOutputError {
    #[error("empty model text")]
    Empty,
    #[error("invalid json: {0}")]
    InvalidJson(String),
    #[error("schema mismatch: expected {expected}, got {action}")]
    ActionMismatch { expected: &'static str, action: String },
    #[error("missing field: {0}")]
    MissingField(&'static str),
    #[error("invalid field: {0}")]
    InvalidField(&'static str),
    #[error("unknown schema: {0}")]
    UnknownSchema(String),
}

/// JSON schema text injected into the system prompt for one phase.
pub fn schema_json_for_name(name: &str) -> Option<&'static str> {
    match name {
        "frontend_intake" => Some(
            r#"{"action":"task_understood|need_clarification","message":"optional string"}"#,
        ),
        "frontend_delegate" => Some(r#"{"action":"delegate_ack"}"#),
        "frontend_watch" => Some(r#"{"action":"watch_ack"}"#),
        "frontend_report" => Some(r#"{"action":"report_ready","summary":"string"}"#),
        "worker_plan" => Some(r#"{"action":"plan_ready","steps":["step-1","step-2"]}"#),
        "worker_act" => Some(r#"{"action":"step_executed","observation":"optional string"}"#),
        "worker_verify" => Some(r#"{"action":"verify_passed|verify_failed","summary":"optional string"}"#),
        "worker_idle" | "frontend_idle" => None,
        _ => None,
    }
}

#[derive(Deserialize)]
struct RawOutput {
    action: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    steps: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)]
    observation: Option<String>,
}

/// Parse plain-text model output against the expected phase schema name.
pub fn parse_and_validate(schema_name: &str, text: &str) -> Result<ValidatedLlmOutput, LlmOutputError> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(LlmOutputError::Empty);
    }

    let raw: RawOutput = serde_json::from_str(trimmed)
        .map_err(|e| LlmOutputError::InvalidJson(e.to_string()))?;

    match schema_name {
        "frontend_intake" => match raw.action.as_str() {
            "task_understood" => Ok(ValidatedLlmOutput::FrontendIntake {
                action: FrontendIntakeAction::TaskUnderstood,
                message: raw.message,
            }),
            "need_clarification" => Ok(ValidatedLlmOutput::FrontendIntake {
                action: FrontendIntakeAction::NeedClarification,
                message: raw.message,
            }),
            other => Err(LlmOutputError::ActionMismatch {
                expected: "task_understood|need_clarification",
                action: other.to_string(),
            }),
        },
        "frontend_report" => {
            if raw.action != "report_ready" {
                return Err(LlmOutputError::ActionMismatch {
                    expected: "report_ready",
                    action: raw.action,
                });
            }
            let summary = raw
                .summary
                .filter(|s| !s.is_empty())
                .ok_or(LlmOutputError::MissingField("summary"))?;
            Ok(ValidatedLlmOutput::FrontendReport { summary })
        }
        "worker_plan" => {
            if raw.action != "plan_ready" {
                return Err(LlmOutputError::ActionMismatch {
                    expected: "plan_ready",
                    action: raw.action,
                });
            }
            if raw.steps.is_empty() {
                return Err(LlmOutputError::MissingField("steps"));
            }
            let mut steps = Vec::with_capacity(raw.steps.len());
            for step in raw.steps {
                if step.trim().is_empty() {
                    return Err(LlmOutputError::InvalidField("steps"));
                }
                steps.push(
                    StepId::from_wire(&step)
                        .map_err(|_| LlmOutputError::InvalidField("steps"))?,
                );
            }
            Ok(ValidatedLlmOutput::WorkerPlan { steps })
        }
        "worker_verify" => match raw.action.as_str() {
            "verify_passed" => Ok(ValidatedLlmOutput::WorkerVerify {
                passed: true,
                summary: raw.summary,
            }),
            "verify_failed" => Ok(ValidatedLlmOutput::WorkerVerify {
                passed: false,
                summary: raw.summary,
            }),
            other => Err(LlmOutputError::ActionMismatch {
                expected: "verify_passed|verify_failed",
                action: other.to_string(),
            }),
        },
        "frontend_delegate" | "frontend_watch" | "worker_act" | "worker_idle" | "frontend_idle" => {
            Err(LlmOutputError::UnknownSchema(schema_name.to_string()))
        }
        other => Err(LlmOutputError::UnknownSchema(other.to_string())),
    }
}

/// Merge validated output into harness iteration for transitions.
pub fn validated_from_plain_text(
    schema_name: &str,
    text: &str,
) -> Option<ValidatedLlmOutput> {
    parse_and_validate(schema_name, text).ok()
}

/// Append OpenAI-style messages into a worker message tail array.
pub fn append_messages(tail: &mut Value, appended: &Value) {
    let Value::Array(messages) = appended else {
        return;
    };
    if messages.is_empty() {
        return;
    }
    match tail {
        Value::Array(tail_arr) => tail_arr.extend(messages.iter().cloned()),
        _ => *tail = Value::Array(messages.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_worker_plan() {
        let out = parse_and_validate(
            "worker_plan",
            r#"{"action":"plan_ready","steps":["step-1","step-2"]}"#,
        )
        .unwrap();
        assert_eq!(
            out,
            ValidatedLlmOutput::WorkerPlan {
                steps: vec![StepId(1), StepId(2)]
            }
        );
    }

    #[test]
    fn rejects_invalid_verify_action() {
        let err = parse_and_validate("worker_verify", r#"{"action":"maybe"}"#).unwrap_err();
        assert!(matches!(err, LlmOutputError::ActionMismatch { .. }));
    }
}
