//! In-memory ingress queues for the orchestrator.

use std::collections::VecDeque;
use std::sync::Mutex;

use super::{AgentEvent, Command, SessionId};

/// User message submitted from an external channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserInput {
    pub session_id: SessionId,
    pub text: String,
}

#[derive(Default)]
pub struct UserInputQueue {
    inner: Mutex<VecDeque<UserInput>>,
}

impl UserInputQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, input: UserInput) {
        self.inner.lock().unwrap().push_back(input);
    }

    pub fn drain(&self) -> Vec<UserInput> {
        let mut guard = self.inner.lock().unwrap();
        guard.drain(..).collect()
    }
}

#[derive(Default)]
pub struct CommandQueue {
    inner: Mutex<VecDeque<Command>>,
}

impl CommandQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, command: Command) {
        self.inner.lock().unwrap().push_back(command);
    }

    pub fn drain(&self) -> Vec<Command> {
        let mut guard = self.inner.lock().unwrap();
        guard.drain(..).collect()
    }
}

#[derive(Default)]
pub struct AgentEventQueue {
    inner: Mutex<VecDeque<AgentEvent>>,
}

impl AgentEventQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, event: AgentEvent) {
        self.inner.lock().unwrap().push_back(event);
    }

    pub fn drain(&self) -> Vec<AgentEvent> {
        let mut guard = self.inner.lock().unwrap();
        guard.drain(..).collect()
    }
}
