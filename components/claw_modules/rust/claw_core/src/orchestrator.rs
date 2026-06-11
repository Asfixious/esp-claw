//! Layer 1 run scheduler: drain queues, tick agent instances, route events.

use std::collections::HashMap;
use std::sync::Arc;

use claw_api::ClawApi;
use claw_cap::CapabilityInvoker;

use crate::agent_spec::{
    apply_patches, AgentRole, AgentState, ContextBuildInput, RoleState, RunStatus,
    TransitionInput, TransitionSignal,
};
use crate::agents::{FrontendAgentSpec, WorkerAgentSpec};
use crate::memory::{snapshot_for_role, InMemoryScopedStore};
use crate::observability::{hash_context, trace_from_iteration, InMemoryTraceSink, SpanName, TraceSink};
use crate::protocol::{
    AgentEvent, AgentEventQueue, Command, CommandQueue, SessionId, TaskContract, TaskId, TaskStatus,
    UserInput, UserInputQueue, WorkerId,
};
use crate::agent_registry::AgentRegistry;
use crate::request::RequestItem;
use crate::llm_output::{append_messages, parse_and_validate};
use crate::runtime::{run_iteration, ActionSummary, HarnessIterationOutput, InstanceControl};

const DEFAULT_ENV_MANIFEST: &str = "target=host-dev\nnetwork=limited";
const DEFAULT_GLOBAL_POLICY: &str = "Follow task contract and visible tools only.";
const DEFAULT_MAX_LLM_ROUNDS_PER_TICK: u32 = 1;

/// Scheduler tuning for one [`RunOrchestrator`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OrchestratorConfig {
    /// Max Layer 3 rounds per agent instance in a single [`RunOrchestrator::tick`].
    pub max_llm_rounds_per_instance_per_tick: u32,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            max_llm_rounds_per_instance_per_tick: DEFAULT_MAX_LLM_ROUNDS_PER_TICK,
        }
    }
}

/// One running agent instance.
pub struct AgentInstance {
    pub state: AgentState,
    pub control: Arc<InstanceControl>,
    pub request: RequestItem,
    next_request_id: u32,
}

impl AgentInstance {
    pub fn frontend(instance_id: impl Into<String>, run_id: impl Into<String>, session_id: SessionId) -> Self {
        let instance_id = instance_id.into();
        Self {
            state: AgentState::frontend(instance_id.clone(), run_id, session_id),
            control: Arc::new(InstanceControl::new()),
            request: RequestItem {
                session_id: Some(instance_id),
                ..Default::default()
            },
            next_request_id: 1,
        }
    }

    pub fn worker(worker_id: WorkerId, run_id: impl Into<String>, task_id: TaskId) -> Self {
        let instance_id = worker_id.to_string();
        Self {
            state: AgentState::worker(instance_id.clone(), run_id, task_id),
            control: Arc::new(InstanceControl::new()),
            request: RequestItem {
                session_id: Some(instance_id),
                ..Default::default()
            },
            next_request_id: 1,
        }
    }

    fn bump_request_id(&mut self) {
        self.request.request_id = self.next_request_id;
        self.next_request_id += 1;
    }
}

/// Multi-agent runtime orchestrator.
pub struct RunOrchestrator {
    pub registry: AgentRegistry,
    pub user_inputs: UserInputQueue,
    pub commands: CommandQueue,
    pub events: AgentEventQueue,
    pub tasks: HashMap<TaskId, TaskContract>,
    pub instances: Vec<AgentInstance>,
    pub memory: Arc<InMemoryScopedStore>,
    pub traces: Arc<InMemoryTraceSink>,
    pub llm: Option<Arc<ClawApi>>,
    pub capability_invoker: Option<Arc<dyn CapabilityInvoker>>,
    env_manifest: String,
    global_policy: String,
    config: OrchestratorConfig,
}

impl RunOrchestrator {
    pub fn new() -> Self {
        let mut registry = AgentRegistry::new();
        registry.register(Arc::new(FrontendAgentSpec::new()));
        registry.register(Arc::new(WorkerAgentSpec::new()));
        Self {
            registry,
            user_inputs: UserInputQueue::new(),
            commands: CommandQueue::new(),
            events: AgentEventQueue::new(),
            tasks: HashMap::new(),
            instances: Vec::new(),
            memory: Arc::new(InMemoryScopedStore::new()),
            traces: Arc::new(InMemoryTraceSink::new()),
            llm: None,
            capability_invoker: None,
            env_manifest: DEFAULT_ENV_MANIFEST.to_string(),
            global_policy: DEFAULT_GLOBAL_POLICY.to_string(),
            config: OrchestratorConfig::default(),
        }
    }

    pub fn with_config(mut self, config: OrchestratorConfig) -> Self {
        self.config = config;
        self
    }

    pub fn with_max_llm_rounds_per_tick(mut self, max_rounds: u32) -> Self {
        self.config.max_llm_rounds_per_instance_per_tick = max_rounds.max(1);
        self
    }

    pub fn with_llm(mut self, llm: Arc<ClawApi>, invoker: Option<Arc<dyn CapabilityInvoker>>) -> Self {
        self.llm = Some(llm);
        self.capability_invoker = invoker;
        self
    }

    pub fn add_instance(&mut self, instance: AgentInstance) {
        self.instances.push(instance);
    }

    pub fn submit_user_input(&self, input: UserInput) {
        self.user_inputs.push(input);
    }

    pub fn submit_command(&self, command: Command) {
        self.commands.push(command);
    }

    pub fn submit_event(&self, event: AgentEvent) {
        self.events.push(event);
    }

    /// One scheduler tick: drain queues, then tick each runnable instance once.
    pub fn tick(&mut self) {
        self.drain_user_inputs();
        self.drain_commands();
        self.drain_events();

        let ids: Vec<String> = self
            .instances
            .iter()
            .filter(|i| i.state.run_status == RunStatus::Runnable)
            .map(|i| i.state.instance_id.clone())
            .collect();

        for id in ids {
            self.tick_instance(&id);
        }
    }

    fn drain_user_inputs(&mut self) {
        for input in self.user_inputs.drain() {
            if let Some(instance) = self
                .instances
                .iter_mut()
                .find(|i| matches!(&i.state.role_state, RoleState::Frontend(f) if f.session_id == input.session_id))
            {
                if let RoleState::Frontend(frontend) = &mut instance.state.role_state {
                    frontend.pending_user_text.push(input.text);
                }
            }
        }
    }

    fn drain_commands(&mut self) {
        let commands = self.commands.drain();
        for command in commands {
            self.handle_command(command);
        }
    }

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::CreateTask {
                task_id,
                goal,
                frontend_instance_id,
                requires_plan_approval,
            } => {
                let run_id = self
                    .instances
                    .iter()
                    .find(|i| i.state.instance_id == frontend_instance_id)
                    .map(|i| i.state.run_id.clone())
                    .unwrap_or_else(|| "run-0".to_string());
                let mut task = TaskContract::new(
                    task_id,
                    run_id.clone(),
                    goal,
                    frontend_instance_id.clone(),
                    &self.env_manifest,
                );
                task.status = TaskStatus::Active;
                task.requires_plan_approval = requires_plan_approval;
                let worker_id = WorkerId::for_task(task_id);
                task.worker_instance_id = Some(worker_id);
                self.tasks.insert(task_id, task);

                let task_id_for_signal = task_id;
                self.instances
                    .push(AgentInstance::worker(worker_id, run_id, task_id));

                if let Some(frontend) = self.instances.iter_mut().find(|i| i.state.instance_id == frontend_instance_id) {
                    let spec = self.registry.get(AgentRole::Frontend).expect("frontend spec");
                    let input = TransitionInput {
                        signal: Some(TransitionSignal::TaskCreated {
                            task_id: task_id_for_signal,
                            worker_id,
                        }),
                        ..Default::default()
                    };
                    let out = spec.transition(&frontend.state, &input);
                    apply_patches(&mut frontend.state, &out.patches);
                    for event in out.events {
                        self.events.push(event);
                    }
                }
            }
            Command::ApprovePlan { task_id, approval } => {
                if approval.approved {
                    if let Some(instance) = self.instances.iter_mut().find(|i| {
                        matches!(&i.state.role_state, RoleState::Worker(w) if w.task_id == task_id)
                    }) {
                        if let RoleState::Worker(worker) = &instance.state.role_state {
                            if worker.awaiting_plan_approval {
                                apply_patches(
                                    &mut instance.state,
                                    &[
                                        crate::agent_spec::StatePatch::SetAwaitingPlanApproval(false),
                                        crate::agent_spec::StatePatch::SetWorkerPhase(
                                            crate::agent_spec::WorkerPhase::Act,
                                        ),
                                    ],
                                );
                                instance.state.run_status = RunStatus::Runnable;
                            }
                        }
                    }
                    if let Some(task) = self.tasks.get_mut(&task_id) {
                        task.status = TaskStatus::Active;
                    }
                } else if let Some(note) = approval.note {
                    self.handle_command(Command::RequestPlanRevision { task_id, note });
                }
            }
            Command::RequestPlanRevision { task_id, note } => {
                if let Some(instance) = self.instances.iter_mut().find(|i| {
                    matches!(&i.state.role_state, RoleState::Worker(w) if w.task_id == task_id)
                }) {
                    apply_patches(
                        &mut instance.state,
                        &[
                            crate::agent_spec::StatePatch::SetPlanSteps(Vec::new()),
                            crate::agent_spec::StatePatch::SetWorkerPhase(
                                crate::agent_spec::WorkerPhase::Plan,
                            ),
                            crate::agent_spec::StatePatch::SetRevisionNote(Some(note.clone())),
                            crate::agent_spec::StatePatch::SetAwaitingPlanApproval(false),
                        ],
                    );
                    instance.state.run_status = RunStatus::Runnable;
                }
                if let Some(task) = self.tasks.get_mut(&task_id) {
                    task.status = TaskStatus::Draft;
                }
                if let Some(frontend_id) = self.tasks.get(&task_id).map(|t| t.frontend_instance_id.clone()) {
                    if let Some(frontend) = self
                        .instances
                        .iter_mut()
                        .find(|i| i.state.instance_id == frontend_id)
                    {
                        apply_patches(
                            &mut frontend.state,
                            &[crate::agent_spec::StatePatch::SetReportDraft(format!(
                                "plan revision requested: {note}"
                            ))],
                        );
                    }
                }
            }
            Command::ApprovalResponse { task_id, approval } => {
                if approval.approved {
                    if let Some(instance) = self.instances.iter_mut().find(|i| {
                        matches!(&i.state.role_state, RoleState::Worker(w) if w.task_id == task_id)
                    }) {
                        if let RoleState::Worker(worker) = &instance.state.role_state {
                            if worker.phase == crate::agent_spec::WorkerPhase::Paused {
                                apply_patches(
                                    &mut instance.state,
                                    &[crate::agent_spec::StatePatch::SetWorkerPhase(
                                        crate::agent_spec::WorkerPhase::Act,
                                    )],
                                );
                                instance.state.run_status = RunStatus::Runnable;
                            }
                        }
                    }
                    if let Some(frontend) = self.instances.iter_mut().find(|i| {
                        matches!(&i.state.role_state, RoleState::Frontend(f) if f.task_id == Some(task_id))
                    }) {
                        let spec = self.registry.get(AgentRole::Frontend).expect("frontend spec");
                        let out = spec.transition(
                            &frontend.state,
                            &TransitionInput {
                                signal: Some(TransitionSignal::UserApproved),
                                ..Default::default()
                            },
                        );
                        apply_patches(&mut frontend.state, &out.patches);
                    }
                }
                let _ = approval;
            }
            Command::CancelRun { run_id } => {
                for instance in &mut self.instances {
                    if instance.state.run_id == run_id {
                        instance.state.run_status = RunStatus::Cancelled;
                    }
                }
            }
        }
    }

    fn drain_events(&mut self) {
        let events = self.events.drain();
        for event in events {
            self.route_event(event);
        }
    }

    fn route_event(&mut self, event: AgentEvent) {
        let task_id = event.task_id();
        for instance in &mut self.instances {
            if let RoleState::Frontend(frontend) = &instance.state.role_state {
                if frontend.task_id == Some(task_id) {
                    let spec = self.registry.get(AgentRole::Frontend).expect("frontend spec");
                    let out = spec.handle_event(&instance.state, &event);
                    apply_patches(&mut instance.state, &out.patches);
                }
            }
        }
    }

    fn tick_instance(&mut self, instance_id: &str) {
        let max_rounds = self.config.max_llm_rounds_per_instance_per_tick;
        for _ in 0..max_rounds {
            if !self.tick_instance_once(instance_id) {
                break;
            }
        }
    }

    /// Run at most one Layer 3 round for `instance_id`. Returns true if a round ran.
    fn tick_instance_once(&mut self, instance_id: &str) -> bool {
        let idx = match self.instances.iter().position(|i| i.state.instance_id == instance_id) {
            Some(i) => i,
            None => return false,
        };

        let role = self.instances[idx].state.role;
        let spec = match self.registry.get(role) {
            Some(s) => Arc::clone(&s),
            None => return false,
        };

        if !spec.should_call_llm(&self.instances[idx].state) {
            return false;
        }

        let llm = match &self.llm {
            Some(l) => Arc::clone(l),
            None => return false,
        };

        let task = self.task_for_instance(idx);
        let requires_plan_approval = task
            .as_ref()
            .map(|t| t.requires_plan_approval)
            .unwrap_or(false);
        let memory_snapshot = snapshot_for_role(
            self.memory.as_ref(),
            role,
            task.as_ref().map(|t| t.task_id),
        );

        let build_input = ContextBuildInput {
            state: &self.instances[idx].state,
            task: task.as_ref(),
            env_manifest: &self.env_manifest,
            global_policy: &self.global_policy,
            memory_snapshot: &memory_snapshot,
        };

        let schema_name = spec.expected_schema(&build_input).name;
        let tools = spec.visible_tools(&build_input);
        let bundle = spec.build_context(&build_input);
        let context_hash = hash_context(&bundle.system_prompt, &bundle.messages.to_string());
        let visible_tools: Vec<String> = tools
            .tools_json
            .as_ref()
            .map(|_| vec!["files".to_string()])
            .unwrap_or_default();

        self.instances[idx].bump_request_id();
        let request = self.instances[idx].request.clone();
        let control = Arc::clone(&self.instances[idx].control);

        let mut output = match run_iteration(
            llm.as_ref(),
            self.capability_invoker.as_deref(),
            control.as_ref(),
            &request,
            &bundle,
        ) {
            Ok(output) => output,
            Err(_) => return false,
        };

        if let ActionSummary::PlainText { text } = &output.action {
            if let Ok(validated) = parse_and_validate(&schema_name, text) {
                output.validated = Some(validated);
            }
        }

        self.merge_worker_messages(idx, &output);

        let mut spans = vec![SpanName::BuildContext, SpanName::Transition];
        spans.extend(output.spans.iter().copied());

        let transition_input = TransitionInput {
            iteration: Some(output),
            signal: None,
            requires_plan_approval,
        };

        let state_snapshot = self.instances[idx].state.clone();
        let phase = state_snapshot.semantic_phase();
        let current_step = match &state_snapshot.role_state {
            RoleState::Worker(w) => w.current_step_id().map(|s| s.to_string()),
            _ => None,
        };

        let transition_out = spec.transition(&state_snapshot, &transition_input);
        apply_patches(&mut self.instances[idx].state, &transition_out.patches);
        apply_patches(
            &mut self.instances[idx].state,
            &[crate::agent_spec::StatePatch::BumpIterationId],
        );

        if let Some(worker_id) = self.worker_id_for_index(idx) {
            for event in transition_out.events {
                let mut event = event;
                self.fill_worker_id(&mut event, worker_id);
                self.route_event(event);
            }
        }

        let trace = trace_from_iteration(
            &self.instances[idx].state.run_id,
            instance_id,
            role,
            self.instances[idx].state.iteration_id,
            phase,
            current_step,
            context_hash,
            visible_tools,
            transition_input.iteration.as_ref(),
            &transition_out.patches,
            spans,
        );
        self.traces.record_iteration(trace);
        true
    }

    fn merge_worker_messages(&mut self, idx: usize, output: &HarnessIterationOutput) {
        if let RoleState::Worker(worker) = &mut self.instances[idx].state.role_state {
            append_messages(&mut worker.message_tail, &output.appended_messages.0);
        }
    }

    fn task_for_instance(&self, idx: usize) -> Option<TaskContract> {
        let state = &self.instances[idx].state;
        let task_id = match &state.role_state {
            RoleState::Frontend(f) => f.task_id,
            RoleState::Worker(w) => Some(w.task_id),
        }?;
        self.tasks.get(&task_id).cloned()
    }

    fn worker_id_for_index(&self, idx: usize) -> Option<WorkerId> {
        match &self.instances[idx].state.role_state {
            RoleState::Worker(worker) => Some(WorkerId::for_task(worker.task_id)),
            _ => None,
        }
    }

    fn fill_worker_id(&self, event: &mut AgentEvent, worker_id: WorkerId) {
        match event {
            AgentEvent::PlanProposed { worker_instance_id, .. }
            | AgentEvent::Progress { worker_instance_id, .. }
            | AgentEvent::ApprovalRequested { worker_instance_id, .. }
            | AgentEvent::Blocked { worker_instance_id, .. }
            | AgentEvent::Done { worker_instance_id, .. } => {
                if worker_instance_id.is_none() {
                    *worker_instance_id = Some(worker_id);
                }
            }
        }
    }
}

impl Default for RunOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}
