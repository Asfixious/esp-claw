#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Integration tests for `BaseAgent`'s human-in-the-loop approval flow.
//!
//! These exercise the built-in `request_approval` tool: the agent pauses into
//! `AwaitingApproval`, a human resolves (or cancels) the pending decision, and
//! the recorded transcript / resumed run reflect that decision exactly.

mod common;

use claw_core::agent::{
    AgentCommandError, AgentId, AgentState, ApprovalDecision, ApprovalId, CancelReason, TickOutcome,
};
use common::{
    agent_builder, body_plain_text, body_request_approval, builder_with_view, capturing_llm,
    scripted_llm, transcript_contents, TestAgent,
};

/// Build an agent over fresh disk memory with the given scripted LLM.
fn build_agent(name: &str, llm: claw_api::ClawApi) -> TestAgent {
    let dir = common::test_output_dir(name);
    agent_builder(llm, AgentId(1), dir.display().to_string())
        .build()
        .expect("build")
}

// ---------------------------------------------------------------------------

#[test]
fn request_approval_pauses_for_decision() {
    let mut agent = build_agent(
        "appr_request_pauses",
        scripted_llm(vec![body_request_approval("delete prod")]),
    );

    agent.run("do it");
    assert!(matches!(
        agent.tick(),
        TickOutcome::AwaitingApproval { ref summary, .. } if summary == "delete prod"
    ));
    assert!(!agent.is_running());
}

#[test]
fn approve_resumes_and_records_decision() {
    let dir = common::test_output_dir("appr_approve_records");
    let (builder, view) = builder_with_view(
        scripted_llm(vec![
            body_request_approval("do it"),
            body_plain_text("done"),
        ]),
        AgentId(1),
        dir.display().to_string(),
    );
    let mut agent = builder.build().expect("build");

    agent.run("go");
    let id = match agent.tick() {
        TickOutcome::AwaitingApproval { id, summary } => {
            assert_eq!(summary, "do it");
            id
        }
        other => panic!("expected AwaitingApproval, got {other:?}"),
    };

    agent
        .resolve_approval(id, ApprovalDecision::Approved)
        .expect("resolve accepted");

    assert!(matches!(agent.tick(), TickOutcome::Yielded { text } if text == "done"));
    let transcript = transcript_contents(&view);
    assert!(transcript
        .iter()
        .any(|c| c.contains("approved by the human")));
}

#[test]
fn reject_resumes_and_records_reason() {
    let dir = common::test_output_dir("appr_reject_records");
    let (builder, view) = builder_with_view(
        scripted_llm(vec![body_request_approval("do it"), body_plain_text("ok")]),
        AgentId(1),
        dir.display().to_string(),
    );
    let mut agent = builder.build().expect("build");

    agent.run("go");
    let id = match agent.tick() {
        TickOutcome::AwaitingApproval { id, summary } => {
            assert_eq!(summary, "do it");
            id
        }
        other => panic!("expected AwaitingApproval, got {other:?}"),
    };

    agent
        .resolve_approval(id, ApprovalDecision::Rejected("too risky".into()))
        .expect("resolve accepted");

    assert!(matches!(agent.tick(), TickOutcome::Yielded { text } if text == "ok"));
    let transcript = transcript_contents(&view);
    assert!(transcript
        .iter()
        .any(|c| c.contains("rejected by the human") && c.contains("too risky")));
}

#[test]
fn wrong_approval_id_is_rejected_and_stays_awaiting() {
    let mut agent = build_agent(
        "appr_wrong_id",
        scripted_llm(vec![body_request_approval("x"), body_plain_text("after")]),
    );

    agent.run("go");
    let id = match agent.tick() {
        TickOutcome::AwaitingApproval { id, summary } => {
            assert_eq!(summary, "x");
            id
        }
        other => panic!("expected AwaitingApproval, got {other:?}"),
    };

    assert_eq!(
        agent.resolve_approval(ApprovalId(999), ApprovalDecision::Approved),
        Err(AgentCommandError::ApprovalMismatch {
            expected: id,
            got: ApprovalId(999),
        })
    );

    // Still awaiting: no iteration runs, no scripted body is consumed.
    assert!(matches!(agent.tick(), TickOutcome::Idle));

    agent
        .resolve_approval(id, ApprovalDecision::Approved)
        .expect("resolve accepted");
    assert!(matches!(agent.tick(), TickOutcome::Yielded { text } if text == "after"));
}

#[test]
fn approve_twice_is_rejected() {
    let mut agent = build_agent(
        "appr_approve_twice",
        scripted_llm(vec![body_request_approval("x")]),
    );

    agent.run("go");
    let id = match agent.tick() {
        TickOutcome::AwaitingApproval { id, summary } => {
            assert_eq!(summary, "x");
            id
        }
        other => panic!("expected AwaitingApproval, got {other:?}"),
    };

    agent
        .resolve_approval(id, ApprovalDecision::Approved)
        .expect("first resolve accepted");

    // Projected state is now Running, so a second resolve is illegal.
    assert_eq!(
        agent.resolve_approval(id, ApprovalDecision::Approved),
        Err(AgentCommandError::NotAwaitingApproval {
            state: AgentState::Running
        })
    );
}

#[test]
fn resolve_when_idle_is_rejected() {
    let mut agent = build_agent("appr_resolve_idle", scripted_llm(vec![]));

    assert_eq!(
        agent.resolve_approval(ApprovalId(0), ApprovalDecision::Approved),
        Err(AgentCommandError::NotAwaitingApproval {
            state: AgentState::Idle
        })
    );
}

#[test]
fn cancel_while_awaiting_clears_pending_and_records_marker() {
    let dir = common::test_output_dir("appr_cancel_awaiting");
    let (builder, view) = builder_with_view(
        scripted_llm(vec![body_request_approval("x")]),
        AgentId(1),
        dir.display().to_string(),
    );
    let mut agent = builder.build().expect("build");

    agent.run("go");
    let id = match agent.tick() {
        TickOutcome::AwaitingApproval { id, summary } => {
            assert_eq!(summary, "x");
            id
        }
        other => panic!("expected AwaitingApproval, got {other:?}"),
    };

    agent
        .cancel(CancelReason::UserRequested)
        .expect("cancel accepted");

    assert!(matches!(
        agent.tick(),
        TickOutcome::Cancelled {
            reason: CancelReason::UserRequested
        }
    ));

    let transcript = transcript_contents(&view);
    assert!(transcript.iter().any(|c| c.contains("interrupted")));

    // Pending approval cleared; the agent is Idle again.
    assert_eq!(
        agent.resolve_approval(id, ApprovalDecision::Approved),
        Err(AgentCommandError::NotAwaitingApproval {
            state: AgentState::Idle
        })
    );
}

#[test]
fn append_while_awaiting_is_included_after_approval() {
    let dir = common::test_output_dir("appr_append_awaiting");
    let (llm, http) = capturing_llm(vec![body_request_approval("x"), body_plain_text("final")]);
    let mut agent = agent_builder(llm, AgentId(1), dir.display().to_string())
        .build()
        .expect("build");

    agent.run("go");
    let id = match agent.tick() {
        TickOutcome::AwaitingApproval { id, summary } => {
            assert_eq!(summary, "x");
            id
        }
        other => panic!("expected AwaitingApproval, got {other:?}"),
    };

    agent.append_message("more info");
    agent
        .resolve_approval(id, ApprovalDecision::Approved)
        .expect("resolve accepted");

    assert!(matches!(agent.tick(), TickOutcome::Yielded { text } if text == "final"));

    assert_eq!(http.call_count(), 2);
    let second_body = http.captured_bodies()[1].to_string();
    assert!(second_body.contains("more info"));
    assert!(second_body.contains("approved by the human"));
}
