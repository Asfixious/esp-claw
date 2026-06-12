use std::sync::{Arc, Mutex};

use claw_message_channel::{
    InboundMessage, MessageChannel, MessageChannelHub, MessageError, OutboundMessage,
};

struct MemoryChannel {
    id: String,
    sent: Arc<Mutex<Vec<OutboundMessage>>>,
}

impl MemoryChannel {
    fn new(id: impl Into<String>) -> (Arc<Self>, Arc<Mutex<Vec<OutboundMessage>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let channel = Arc::new(Self {
            id: id.into(),
            sent: Arc::clone(&sent),
        });
        (channel, sent)
    }
}

impl MessageChannel for MemoryChannel {
    fn id(&self) -> &str {
        &self.id
    }

    fn send(&self, msg: &OutboundMessage) -> Result<(), MessageError> {
        self.sent.lock().unwrap().push(msg.clone());
        Ok(())
    }
}

fn drain_sent(sent: &Arc<Mutex<Vec<OutboundMessage>>>) -> Vec<OutboundMessage> {
    let mut guard = sent.lock().unwrap();
    guard.drain(..).collect()
}

#[test]
fn hub_routes_outbound_to_registered_channel() {
    let hub = MessageChannelHub::new();
    let (cli, cli_sent) = MemoryChannel::new("cli");
    let (web, web_sent) = MemoryChannel::new("web");
    hub.register(Arc::clone(&cli) as Arc<dyn MessageChannel>);
    hub.register(Arc::clone(&web) as Arc<dyn MessageChannel>);

    hub.send(OutboundMessage {
        channel: "cli".into(),
        chat_id: "local".into(),
        text: "hello cli".into(),
        reply_to_message_id: None,
    })
    .unwrap();

    hub.send(OutboundMessage {
        channel: "web".into(),
        chat_id: "room-1".into(),
        text: "hello web".into(),
        reply_to_message_id: Some("m-9".into()),
    })
    .unwrap();

    let cli_msgs = drain_sent(&cli_sent);
    let web_msgs = drain_sent(&web_sent);
    assert_eq!(cli_msgs.len(), 1);
    assert_eq!(web_msgs.len(), 1);
    assert_eq!(cli_msgs[0].text, "hello cli");
    assert_eq!(web_msgs[0].text, "hello web");
}

#[test]
fn hub_buffers_and_drains_inbound() {
    let hub = MessageChannelHub::new();
    hub.submit_inbound(InboundMessage {
        message_id: "m-1".into(),
        channel: "cli".into(),
        chat_id: "local".into(),
        sender_id: None,
        session_id: "session-1".into(),
        text: "hi".into(),
    });
    hub.submit_inbound(InboundMessage {
        message_id: "m-2".into(),
        channel: "web".into(),
        chat_id: "room-1".into(),
        sender_id: Some("user-1".into()),
        session_id: "session-2".into(),
        text: "hey".into(),
    });

    let drained = hub.drain_inbound();
    assert_eq!(drained.len(), 2);
    assert!(hub.drain_inbound().is_empty());
}

#[test]
fn hub_send_to_session_uses_latest_inbound_route() {
    let hub = MessageChannelHub::new();
    let (cli, cli_sent) = MemoryChannel::new("cli");
    let (web, web_sent) = MemoryChannel::new("web");
    hub.register(Arc::clone(&cli) as Arc<dyn MessageChannel>);
    hub.register(Arc::clone(&web) as Arc<dyn MessageChannel>);

    hub.submit_inbound(InboundMessage {
        message_id: "m-1".into(),
        channel: "cli".into(),
        chat_id: "local".into(),
        sender_id: None,
        session_id: "session-1".into(),
        text: "hi".into(),
    });
    hub.send_to_session("session-1", "reply on cli").unwrap();

    hub.submit_inbound(InboundMessage {
        message_id: "m-2".into(),
        channel: "web".into(),
        chat_id: "room-9".into(),
        sender_id: Some("user-1".into()),
        session_id: "session-1".into(),
        text: "now on web".into(),
    });
    hub.send_to_session("session-1", "reply on web").unwrap();

    let cli_msgs = drain_sent(&cli_sent);
    let web_msgs = drain_sent(&web_sent);
    assert_eq!(cli_msgs.len(), 1);
    assert_eq!(web_msgs.len(), 1);
    assert_eq!(cli_msgs[0].text, "reply on cli");
    assert_eq!(cli_msgs[0].reply_to_message_id.as_deref(), Some("m-1"));
    assert_eq!(web_msgs[0].text, "reply on web");
    assert_eq!(web_msgs[0].chat_id, "room-9");
    assert_eq!(web_msgs[0].reply_to_message_id.as_deref(), Some("m-2"));
}

#[test]
fn hub_send_to_session_without_inbound_errors() {
    let hub = MessageChannelHub::new();
    let err = hub.send_to_session("unknown", "hi").unwrap_err();
    assert!(matches!(
        err,
        MessageError::SessionRouteNotFound(id) if id == "unknown"
    ));
}

#[test]
fn hub_send_unknown_channel_errors() {
    let hub = MessageChannelHub::new();
    let err = hub
        .send(OutboundMessage {
            channel: "missing".into(),
            chat_id: "x".into(),
            text: "nope".into(),
            reply_to_message_id: None,
        })
        .unwrap_err();
    assert!(matches!(err, MessageError::ChannelNotFound(id) if id == "missing"));
}
