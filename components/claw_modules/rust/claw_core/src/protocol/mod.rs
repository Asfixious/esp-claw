//! Inter-agent protocol: shared task state, commands, and events.

mod command;
mod event;
mod ids;
mod queues;
mod task;

pub use command::Command;
pub use event::AgentEvent;
pub use ids::{IdParseError, SessionId, StepId, TaskId, WorkerId};
pub use queues::{AgentEventQueue, CommandQueue, UserInput, UserInputQueue};
pub use task::{ApprovalRecord, TaskContract, TaskStatus};
