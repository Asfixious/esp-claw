//! Interactive chat CLI backed by the full [`Orchestrator`] (Layer 1).
//!
//! Unlike `generic-agent-chat` (which drives a single [`GenericAgent`]
//! directly), this goes through the real orchestrator path: one session, a
//! per-session agent graph built by [`FsAgentFactory`], replies routed back
//! through the channel egress. The root is a `conversation` agent that can spawn
//! `worker` subagents, so this exercises multi-agent spawning end to end.
//!
//! Capabilities/skills come from each kind's compile-time manifest; the resolver
//! is empty here (the built-in kinds declare no extra capabilities — agents still
//! get their built-in control/spawn tools). Approval requests are surfaced as
//! ordinary messages; this simple loop does not resolve them (matching the other
//! CLIs).
//!
//! LLM config is read from `claw_core/.env.local`; memory is written to
//! `claw_core/output/orchestrator-chat/`.
//!
//! ```
//! cargo run -p claw-agent-cli --bin orchestrator-chat --target x86_64-unknown-linux-gnu
//! ```

use std::io::{self, BufRead, Write};
use std::sync::Arc;

use claw_agent_cli::{load_env, make_http, make_llm_config, make_memory_deps};
use claw_core::agent::{FsAgentFactory, MapAgentResolver};
use claw_core::{
    ChannelEgress, ChannelEgressHub, ChannelIngressSink, ChannelTransport, InboundMessage,
    Orchestrator, RecordingTransport, SessionId,
};
use owo_colors::OwoColorize;
use serde_json::Value;

const MEMORY_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../claw_core/output/orchestrator-chat"
);
/// The CLI's single inbound/outbound channel; the transport registers under it
/// and inbound messages carry it so the reply route resolves back to us.
const CHANNEL: &str = "cli";
const CHAT_ID: &str = "cli-chat";

fn main() {
    load_env();

    // Empty resolver: the built-in conversation/worker manifests declare no extra
    // capabilities, so no name->handler mapping is needed yet.
    let resolver = Arc::new(MapAgentResolver::new());
    let factory = Arc::new(FsAgentFactory::new(
        resolver,
        make_llm_config(true),
        make_http(),
        MEMORY_DIR,
        make_memory_deps(),
    ));

    // A recording transport doubles as the CLI's "screen": the orchestrator sends
    // replies through the egress, and we drain them after each turn.
    let transport = RecordingTransport::new(CHANNEL);
    let egress = Arc::new(ChannelEgressHub::new());
    egress.register(Arc::clone(&transport) as Arc<dyn ChannelTransport>);

    let orchestrator = Orchestrator::builder()
        .config_egress(egress as Arc<dyn ChannelEgress>)
        .with_agent_factory(factory)
        .build();
    let session = orchestrator.session_create();

    eprintln!("Memory:  {MEMORY_DIR}");
    eprintln!("Session: {}", session.to_wire());
    eprintln!("Type your message and press Enter. Empty line or Ctrl-D to quit.\n");

    let stdin = io::stdin();
    let mut turn: u64 = 0;
    loop {
        print!("> ");
        io::stdout().flush().expect("flush stdout");

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let input = line.trim();
        if input.is_empty() {
            break;
        }

        // Remember where the root's transcript ends so we can show the tool calls
        // this turn appends (e.g. spawn_subagent, respond_to_approval), after the
        // reply. `None` before the first turn builds the root.
        let turn_start = transcript_len(&orchestrator, session);

        turn += 1;
        orchestrator.push_user_message(InboundMessage {
            message_id: format!("m{turn}"),
            channel: CHANNEL.into(),
            chat_id: CHAT_ID.into(),
            sender_id: None,
            session_id: session.to_wire(),
            text: input.to_string(),
        });

        // The orchestrator drives the graph synchronously inside `push_user_message`
        // and routes every reply/approval through our transport.
        let replies = transport.drain_sent();
        if replies.is_empty() {
            println!("\n(no reply)");
        }
        for reply in replies {
            println!("\n{}", reply.text);
        }

        if let Some(messages) = orchestrator.root_transcript(session) {
            print_tool_calls_since(&messages, turn_start);
        }
        println!();
    }

    eprintln!("Goodbye.");
}

/// Number of messages currently in the session root's transcript (0 before the
/// root exists).
fn transcript_len(orchestrator: &Orchestrator, session: SessionId) -> usize {
    orchestrator
        .root_transcript(session)
        .and_then(|messages| messages.as_array().map(Vec::len))
        .unwrap_or(0)
}

/// Print, in gray under the reply, every tool call recorded after index `start` —
/// the calls the root made while producing this turn's answer.
fn print_tool_calls_since(messages: &Value, start: usize) {
    let Some(items) = messages.as_array() else {
        return;
    };
    for message in items.iter().skip(start) {
        let Some(calls) = message.get("tool_calls").and_then(Value::as_array) else {
            continue;
        };
        for call in calls {
            let function = call.get("function");
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let arguments = match function.and_then(|f| f.get("arguments")) {
                Some(Value::String(s)) => s.clone(),
                Some(value) => value.to_string(),
                None => String::new(),
            };
            println!("{}", format!("  ↳ {name}({arguments})").bright_black());
        }
    }
}
