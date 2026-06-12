//! End-to-end orchestrator tests with a scripted LLM.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use claw_api::{ClawApi, ClawApiConfig};
use claw_core::agent::{AgentRole, FrontendPhase, RoleState, RunStatus, WorkerPhase};
use claw_core::memory::{role_may_read, InMemoryScopedStore, MemoryScope, ScopedMemoryStore};
use claw_core::orchestrator::{AgentInstance, RunOrchestrator};
use claw_core::protocol::{ApprovalRecord, Command, SessionId, StepId, TaskId, WorkerId};
use claw_core::InboundMessage;

const SESS: SessionId = SessionId(1);
const TASK: TaskId = TaskId(1);
use claw_interfaces::http::{ClawHttp, HttpError, HttpJsonRequest, HttpResponse};

const INTAKE_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"{\"action\":\"task_understood\"}"}}]}"#;
const PLAN_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"{\"action\":\"plan_ready\",\"steps\":[\"step-1\",\"step-2\"]}"}}]}"#;
const VERIFY_PASS_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"{\"action\":\"verify_passed\",\"summary\":\"ok\"}"}}]}"#;
const REPORT_BODY: &str = r#"{"choices":[{"message":{"role":"assistant","content":"{\"action\":\"report_ready\",\"summary\":\"all done\"}"}}]}"#;
const TOOL_BODY: &str =
    r#"{"choices":[{"message":{"role":"assistant","tool_calls":[{"id":"t1","function":{"name":"files","arguments":"{}"}}]}}]}"#;

struct ScriptedHttp {
    bodies: Mutex<VecDeque<String>>,
}

impl ClawHttp for ScriptedHttp {
    fn post_json(
        &self,
        _request: &HttpJsonRequest,
        _abort: &std::sync::atomic::AtomicBool,
    ) -> Result<HttpResponse, HttpError> {
        let body = self
            .bodies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| VERIFY_PASS_BODY.into());
        Ok(HttpResponse {
            status_code: 200,
            body,
        })
    }
}

struct EchoCapability;

impl claw_cap::CapabilityInvoker for EchoCapability {
    fn invoke(
        &self,
        capability_name: &str,
        input_json: &str,
        _context: &claw_cap::ToolContext,
    ) -> Result<claw_cap::CapabilityInvokeResult, claw_cap::CapabilityError> {
        Ok(claw_cap::CapabilityInvokeResult {
            output: format!("{capability_name}:{input_json}"),
            ok: true,
        })
    }
}

fn test_llm(bodies: Vec<&str>) -> Arc<ClawApi> {
    let http = Arc::new(ScriptedHttp {
        bodies: Mutex::new(bodies.into_iter().map(str::to_string).collect()),
    });
    Arc::new(
        ClawApi::init(
            ClawApiConfig {
                api_key: Some("sk-test".into()),
                backend_type: "openai_compatible".into(),
                model: Some("gpt-test".into()),
                base_url: Some("https://example.invalid".into()),
                supports_tools: true,
                ..Default::default()
            },
            http,
        )
        .expect("llm init"),
    )
}

#[test]
fn orchestrator_frontend_delegates_worker_completes() {
    let llm = test_llm(vec![
        INTAKE_BODY,
        PLAN_BODY,
        TOOL_BODY,
        VERIFY_PASS_BODY,
        TOOL_BODY,
        VERIFY_PASS_BODY,
        REPORT_BODY,
    ]);
    let invoker = Arc::new(EchoCapability);
    let mut orch = RunOrchestrator::new().with_llm(llm, Some(invoker));

    orch.add_instance(AgentInstance::frontend("fe-1", "run-1", SESS));
    orch.submit_inbound(InboundMessage {
        message_id: "m-1".into(),
        channel: "test".into(),
        chat_id: "local".into(),
        sender_id: None,
        session_id: SESS.to_string(),
        text: "build something".into(),
    });
    orch.tick();

    orch.submit_command(Command::CreateTask {
        task_id: TASK,
        goal: "build something".into(),
        frontend_instance_id: "fe-1".into(),
        requires_plan_approval: false,
    });
    orch.tick();

    let worker = orch
        .instances
        .iter()
        .find(|i| matches!(&i.state.role_state, RoleState::Worker(_)))
        .expect("worker spawned");
    if let RoleState::Worker(w) = &worker.state.role_state {
        assert_eq!(w.task_id, TASK);
        assert!(!matches!(w.phase, WorkerPhase::Plan));
    } else {
        panic!("expected worker");
    }

    for _ in 0..10 {
        orch.tick();
    }

    let worker = orch
        .instances
        .iter()
        .find(|i| matches!(&i.state.role_state, RoleState::Worker(_)))
        .expect("worker");
    if let RoleState::Worker(w) = &worker.state.role_state {
        assert_eq!(w.phase, WorkerPhase::Done);
    }

    let frontend = orch
        .instances
        .iter()
        .find(|i| i.state.instance_id == "fe-1")
        .expect("frontend");
    if let RoleState::Frontend(f) = &frontend.state.role_state {
        assert!(
            matches!(f.phase, FrontendPhase::Report | FrontendPhase::Done),
            "frontend should reach report or done, got {:?}",
            f.phase
        );
    }

    let traces = orch.traces.all_iterations("run-1");
    assert!(!traces.is_empty());
}

#[test]
fn orchestrator_approval_unblocks_paused_worker() {
    let mut orch = RunOrchestrator::new();
    orch.add_instance(AgentInstance::frontend("fe-1", "run-1", SESS));
    let mut worker = AgentInstance::worker(WorkerId::for_task(TASK), "run-1", TASK);
    if let RoleState::Worker(w) = &mut worker.state.role_state {
        w.phase = WorkerPhase::Paused;
    }
    worker.state.run_status = RunStatus::Waiting;
    orch.add_instance(worker);

    orch.submit_command(Command::ApprovalResponse {
        task_id: TASK,
        approval: ApprovalRecord {
            task_id: TASK,
            step_id: Some(StepId(1)),
            approved: true,
            note: None,
        },
    });
    orch.tick();

    let worker = orch
        .instances
        .iter()
        .find(|i| i.state.instance_id == WorkerId::for_task(TASK).to_string())
        .unwrap();
    if let RoleState::Worker(w) = &worker.state.role_state {
        assert_eq!(w.phase, WorkerPhase::Act);
    }
    assert_eq!(worker.state.run_status, RunStatus::Runnable);
}

#[test]
fn memory_scope_denies_cross_role_private_reads() {
    assert!(!role_may_read(
        AgentRole::Frontend,
        MemoryScope::WorkerPrivate
    ));
    assert!(!role_may_read(
        AgentRole::Worker,
        MemoryScope::FrontendPrivate
    ));
    assert!(role_may_read(
        AgentRole::Worker,
        MemoryScope::TaskShared
    ));

    let store = InMemoryScopedStore::new();
    store.write(
        MemoryScope::WorkerPrivate,
        "worker.notes",
        "secret".into(),
    );
    let snap = claw_core::memory::snapshot_for_role(
        &store,
        AgentRole::Frontend,
        Some(TASK),
    );
    assert!(!snap.contains("secret"));
}

#[test]
fn orchestrator_merges_worker_message_tail_after_tool_round() {
    let llm = test_llm(vec![INTAKE_BODY, PLAN_BODY, TOOL_BODY]);
    let invoker = Arc::new(EchoCapability);
    let mut orch = RunOrchestrator::new().with_llm(llm, Some(invoker));

    orch.add_instance(AgentInstance::frontend("fe-1", "run-1", SESS));
    orch.submit_inbound(InboundMessage {
        message_id: "m-1".into(),
        channel: "test".into(),
        chat_id: "local".into(),
        sender_id: None,
        session_id: SESS.to_string(),
        text: "build".into(),
    });
    orch.tick();
    orch.submit_command(Command::CreateTask {
        task_id: TASK,
        goal: "build".into(),
        frontend_instance_id: "fe-1".into(),
        requires_plan_approval: false,
    });
    for _ in 0..4 {
        orch.tick();
    }

    let worker = orch
        .instances
        .iter()
        .find(|i| matches!(&i.state.role_state, RoleState::Worker(_)))
        .expect("worker");
    if let RoleState::Worker(w) = &worker.state.role_state {
        let tail = w.message_tail.as_array().expect("message tail array");
        assert!(
            tail.len() >= 2,
            "expected assistant+tool messages in tail, got {tail:?}"
        );
    } else {
        panic!("expected worker");
    }
}

#[test]
fn orchestrator_approve_plan_unblocks_worker() {
    let llm = test_llm(vec![INTAKE_BODY, PLAN_BODY]);
    let mut orch = RunOrchestrator::new().with_llm(llm, None);
    orch.add_instance(AgentInstance::frontend("fe-1", "run-1", SESS));
    orch.submit_inbound(InboundMessage {
        message_id: "m-1".into(),
        channel: "test".into(),
        chat_id: "local".into(),
        sender_id: None,
        session_id: SESS.to_string(),
        text: "build".into(),
    });
    orch.tick();
    orch.submit_command(Command::CreateTask {
        task_id: TASK,
        goal: "build".into(),
        frontend_instance_id: "fe-1".into(),
        requires_plan_approval: true,
    });
    orch.tick();

    let worker = orch
        .instances
        .iter()
        .find(|i| matches!(&i.state.role_state, RoleState::Worker(_)))
        .expect("worker");
    if let RoleState::Worker(w) = &worker.state.role_state {
        assert_eq!(w.phase, WorkerPhase::Paused);
        assert!(w.awaiting_plan_approval);
    } else {
        panic!("expected worker");
    }

    orch.submit_command(Command::ApprovePlan {
        task_id: TASK,
        approval: ApprovalRecord {
            task_id: TASK,
            step_id: None,
            approved: true,
            note: None,
        },
    });
    orch.llm = None;
    orch.tick();

    let worker = orch
        .instances
        .iter()
        .find(|i| matches!(&i.state.role_state, RoleState::Worker(_)))
        .expect("worker");
    if let RoleState::Worker(w) = &worker.state.role_state {
        assert_eq!(w.phase, WorkerPhase::Act);
        assert!(!w.awaiting_plan_approval);
    } else {
        panic!("expected worker");
    }
}

#[test]
fn orchestrator_request_plan_revision_replans() {
    let mut orch = RunOrchestrator::new();
    orch.add_instance(AgentInstance::frontend("fe-1", "run-1", SESS));
    let mut worker = AgentInstance::worker(WorkerId::for_task(TASK), "run-1", TASK);
    if let RoleState::Worker(w) = &mut worker.state.role_state {
        w.phase = WorkerPhase::Paused;
        w.awaiting_plan_approval = true;
        w.plan_steps = vec![StepId(1)];
    }
    worker.state.run_status = RunStatus::Waiting;
    orch.add_instance(worker);
    orch.tasks.insert(
        TASK,
        claw_core::protocol::TaskContract::new(TASK, "run-1", "goal", "fe-1", "env"),
    );

    orch.submit_command(Command::RequestPlanRevision {
        task_id: TASK,
        note: "add tests".into(),
    });
    orch.tick();

    let worker = orch
        .instances
        .iter()
        .find(|i| i.state.instance_id == WorkerId::for_task(TASK).to_string())
        .unwrap();
    if let RoleState::Worker(w) = &worker.state.role_state {
        assert_eq!(w.phase, WorkerPhase::Plan);
        assert!(w.plan_steps.is_empty());
        assert_eq!(w.revision_note.as_deref(), Some("add tests"));
    } else {
        panic!("expected worker");
    }
}
