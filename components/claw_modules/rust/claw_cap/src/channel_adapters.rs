//! Channel ingress/egress adapters for the orchestrator boundary.

use std::collections::VecDeque;
use std::sync::Mutex;

use claw_core::{
    ChannelError, ChannelIngress, ChannelIngressSink, ChannelTransport, InboundCommand,
    InboundMessage, OutboundMessage,
};

/// Buffers inbound user messages and orchestrator commands from C router / CLI.
pub struct CapChannelIngress {
    user_messages: Mutex<VecDeque<InboundMessage>>,
    commands: Mutex<VecDeque<InboundCommand>>,
}

impl Default for CapChannelIngress {
    fn default() -> Self {
        Self::new()
    }
}

impl CapChannelIngress {
    pub fn new() -> Self {
        Self {
            user_messages: Mutex::new(VecDeque::new()),
            commands: Mutex::new(VecDeque::new()),
        }
    }
}

impl ChannelIngressSink for CapChannelIngress {
    fn push_user_message(&self, msg: InboundMessage) {
        self.user_messages.lock().unwrap().push_back(msg);
    }

    fn push_command(&self, command: InboundCommand) {
        self.commands.lock().unwrap().push_back(command);
    }
}

impl ChannelIngress for CapChannelIngress {
    fn drain_user_messages(&mut self) -> Vec<InboundMessage> {
        self.user_messages.lock().unwrap().drain(..).collect()
    }

    fn drain_commands(&mut self) -> Vec<InboundCommand> {
        self.commands.lock().unwrap().drain(..).collect()
    }
}

/// Routes outbound messages through a registered transport id.
pub struct CapChannelTransport<F>
where
    F: Fn(&OutboundMessage) -> Result<(), ChannelError> + Send + Sync,
{
    channel_id: String,
    send_fn: F,
}

impl<F> CapChannelTransport<F>
where
    F: Fn(&OutboundMessage) -> Result<(), ChannelError> + Send + Sync,
{
    pub fn new(channel_id: impl Into<String>, send_fn: F) -> Self {
        Self {
            channel_id: channel_id.into(),
            send_fn,
        }
    }
}

impl<F> ChannelTransport for CapChannelTransport<F>
where
    F: Fn(&OutboundMessage) -> Result<(), ChannelError> + Send + Sync,
{
    fn id(&self) -> &str {
        &self.channel_id
    }

    fn send(&self, msg: &OutboundMessage) -> Result<(), ChannelError> {
        (self.send_fn)(msg)
    }
}
