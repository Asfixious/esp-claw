//! Multi-channel registry, inbound queue, and per-session reply routing.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};

use crate::channel::MessageChannel;
use crate::envelope::{InboundMessage, OutboundMessage, ReplyRoute};
use crate::error::MessageError;

/// Connects many [`MessageChannel`] adapters, buffers inbound messages, and
/// tracks per-session reply routes (last inbound wins).
#[derive(Default)]
pub struct MessageChannelHub {
    channels: RwLock<HashMap<String, Arc<dyn MessageChannel>>>,
    inbound: Mutex<VecDeque<InboundMessage>>,
    reply_routes: Mutex<HashMap<String, ReplyRoute>>,
}

impl MessageChannelHub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, channel: Arc<dyn MessageChannel>) {
        let id = channel.id().to_string();
        if let Ok(mut channels) = self.channels.write() {
            channels.insert(id, channel);
        }
    }

    /// Called by channel adapters when external text arrives.
    ///
    /// Updates the session reply route before buffering so the next
    /// [`Self::send_to_session`] uses this message's transport target.
    pub fn submit_inbound(&self, msg: InboundMessage) {
        if let Ok(mut routes) = self.reply_routes.lock() {
            routes.insert(msg.session_id.clone(), ReplyRoute::from_inbound(&msg));
        }
        if let Ok(mut inbound) = self.inbound.lock() {
            inbound.push_back(msg);
        }
    }

    pub fn drain_inbound(&self) -> Vec<InboundMessage> {
        let mut inbound = self.inbound.lock().unwrap();
        inbound.drain(..).collect()
    }

    /// Current reply route for `session_id`, if any inbound has been submitted.
    pub fn reply_route(&self, session_id: &str) -> Option<ReplyRoute> {
        self.reply_routes
            .lock()
            .ok()?
            .get(session_id)
            .cloned()
    }

    /// Send `text` to the channel/chat from the latest inbound on `session_id`.
    pub fn send_to_session(&self, session_id: &str, text: impl Into<String>) -> Result<(), MessageError> {
        let route = self
            .reply_routes
            .lock()
            .map_err(|_| MessageError::SendFailed("reply route registry poisoned".into()))?
            .get(session_id)
            .cloned()
            .ok_or_else(|| MessageError::SessionRouteNotFound(session_id.to_string()))?;

        self.send(OutboundMessage {
            channel: route.channel,
            chat_id: route.chat_id,
            text: text.into(),
            reply_to_message_id: route.reply_to_message_id,
        })
    }

    /// Route an outbound message to the registered channel by `msg.channel`.
    pub fn send(&self, msg: OutboundMessage) -> Result<(), MessageError> {
        let channels = self
            .channels
            .read()
            .map_err(|_| MessageError::SendFailed("channel registry poisoned".into()))?;
        let channel = channels
            .get(&msg.channel)
            .ok_or_else(|| MessageError::ChannelNotFound(msg.channel.clone()))?;
        channel.send(&msg)
    }
}
