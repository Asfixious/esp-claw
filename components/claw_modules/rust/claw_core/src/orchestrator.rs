//! Layer 1 orchestrator: session registry, ingress, egress reply routing.
//!
//! Inbound logic lives in [`Orchestrator::on_user_message`] and
//! [`Orchestrator::on_command`].

use std::sync::Arc;

use claw_api::ClawApi;

use crate::channels::{
    ChannelEgress, ChannelEgressHub, ChannelIngressSink, InboundCommand, InboundMessage,
};
use crate::protocol::Command;
use crate::session::{
    DeliverError, SessionError, SessionId, SessionMessage, SessionOut, SessionRoutes, SessionStore,
};

pub struct Orchestrator {
    egress: Arc<dyn ChannelEgress>,
    llm: Arc<ClawApi>,
    sessions: SessionStore,
    routes: SessionRoutes,
}

impl Orchestrator {
    // -----------------------------------------------------------------------
    // Inbound callbacks — edit these
    // -----------------------------------------------------------------------

    fn on_user_message(&self, session_id: SessionId, msg: &SessionMessage) {
        unimplemented!()
    }

    fn on_command(&self, session_id: SessionId, cmd: &Command) {
        unimplemented!()
    }

    #[allow(dead_code)]
    fn send_message(
        &self,
        session_id: SessionId,
        text: impl Into<String>,
    ) -> Result<(), DeliverError> {
        let reply_route = self
            .routes
            .get(session_id)
            .ok_or(DeliverError::NoReplyRoute(session_id))?;
        SessionOut::new(self.egress.as_ref(), &reply_route)
            .send_message(text)
            .map_err(Into::into)
    }

    fn deliver_user_message(&self, msg: InboundMessage) -> Result<(), DeliverError> {
        if msg.session_id.trim().is_empty() {
            return Err(DeliverError::MissingSessionId);
        }
        let session_id = SessionId::from_wire(&msg.session_id)
            .map_err(|_| DeliverError::InvalidSessionId(msg.session_id.clone()))?;
        if !self.sessions.contains(session_id) {
            return Err(DeliverError::SessionNotFound(session_id));
        }

        self.routes.update_from_inbound(session_id, &msg);
        let session_msg = SessionMessage::from_inbound(&msg);
        self.on_user_message(session_id, &session_msg);
        Ok(())
    }

    fn deliver_command(&self, inbound: InboundCommand) -> Result<(), DeliverError> {
        let session_id = inbound.session_id;
        if !self.sessions.contains(session_id) {
            return Err(DeliverError::SessionNotFound(session_id));
        }
        if self.routes.get(session_id).is_none() {
            return Err(DeliverError::NoReplyRoute(session_id));
        }
        self.on_command(session_id, &inbound.command);
        Ok(())
    }

    pub fn session_create(&self) -> SessionId {
        self.sessions.create().id
    }

    pub fn session_delete(&self, session_id: SessionId) -> Result<(), SessionError> {
        self.sessions.delete(session_id)?;
        self.routes.remove(session_id);
        Ok(())
    }

    pub fn builder() -> OrchestratorBuilder<ChannelsUnset, LlmUnset> {
        OrchestratorBuilder {
            channels: ChannelsUnset,
            llm: LlmUnset,
        }
    }
}

impl ChannelIngressSink for Orchestrator {
    fn push_user_message(&self, msg: InboundMessage) {
        if let Err(err) = self.deliver_user_message(msg) {
            log::warn!("ingress user message deliver failed: {err}");
        }
    }

    fn push_command(&self, command: InboundCommand) {
        if let Err(err) = self.deliver_command(command) {
            log::warn!("ingress command deliver failed: {err}");
        }
    }
}

impl ChannelIngressSink for Arc<Orchestrator> {
    fn push_user_message(&self, msg: InboundMessage) {
        (**self).push_user_message(msg);
    }

    fn push_command(&self, command: InboundCommand) {
        (**self).push_command(command);
    }
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

pub struct OrchestratorBuilder<Channels, Llm> {
    channels: Channels,
    llm: Llm,
}

pub struct ChannelsUnset;
pub struct ChannelsBuiltin;
pub struct ChannelsEgressOnly {
    egress: Arc<dyn ChannelEgress>,
}
pub struct LlmUnset;
pub struct LlmSet {
    llm: Arc<ClawApi>,
}

mod sealed {
    use super::*;

    pub trait SealedChannelsReady {
        fn materialize(self) -> Arc<dyn ChannelEgress>;
    }

    impl SealedChannelsReady for ChannelsBuiltin {
        fn materialize(self) -> Arc<dyn ChannelEgress> {
            Arc::new(ChannelEgressHub::new())
        }
    }

    impl SealedChannelsReady for ChannelsEgressOnly {
        fn materialize(self) -> Arc<dyn ChannelEgress> {
            self.egress
        }
    }
}

trait ChannelsReady: sealed::SealedChannelsReady {}
impl ChannelsReady for ChannelsBuiltin {}
impl ChannelsReady for ChannelsEgressOnly {}

impl<Llm> OrchestratorBuilder<ChannelsUnset, Llm> {
    pub fn with_builtin_channels(self) -> OrchestratorBuilder<ChannelsBuiltin, Llm> {
        OrchestratorBuilder {
            channels: ChannelsBuiltin,
            llm: self.llm,
        }
    }

    pub fn config_egress(
        self,
        egress: Arc<dyn ChannelEgress>,
    ) -> OrchestratorBuilder<ChannelsEgressOnly, Llm> {
        OrchestratorBuilder {
            channels: ChannelsEgressOnly { egress },
            llm: self.llm,
        }
    }
}

impl<Channels> OrchestratorBuilder<Channels, LlmUnset> {
    pub fn with_llm(self, llm: Arc<ClawApi>) -> OrchestratorBuilder<Channels, LlmSet> {
        OrchestratorBuilder {
            channels: self.channels,
            llm: LlmSet { llm },
        }
    }
}

impl<C> OrchestratorBuilder<C, LlmSet>
where
    C: ChannelsReady,
{
    pub fn build(self) -> Arc<Orchestrator> {
        Arc::new(Orchestrator {
            egress: self.channels.materialize(),
            llm: self.llm.llm,
            sessions: SessionStore::new(),
            routes: SessionRoutes::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claw_api::{ClawApi, ClawApiConfig};
    use claw_interface::NoopHttp;

    fn test_llm() -> Arc<ClawApi> {
        Arc::new(
            ClawApi::init(
                ClawApiConfig {
                    api_key: Some("test-key".into()),
                    model: Some("gpt-4o-mini".into()),
                    backend_type: "openai_compatible".into(),
                    base_url: Some("https://api.example.com/v1".into()),
                    ..Default::default()
                },
                Arc::new(NoopHttp),
            )
            .expect("test llm"),
        )
    }

    #[test]
    fn builds_with_builtin_channels_then_llm() {
        let _ = Orchestrator::builder()
            .with_builtin_channels()
            .with_llm(test_llm())
            .build();
    }
}
