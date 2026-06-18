//! Interactive chat CLI backed by [`ConversationAgent`] with persistent memory.
//!
//! Exercises the conversation agent's Intake -> Verify -> Delegate -> Watch ->
//! Report FSM against a live LLM. Subagent spawning is left disabled (the spawn
//! implementation is still pending), so this drives the single-agent path of the
//! FSM. LLM config is read from `claw_core/.env.local`; memory is written to
//! `claw_core/output/conversation-chat/`.
//!
//! ```
//! cargo run -p claw-agent-cli --bin conversation-agent-chat --target x86_64-unknown-linux-gnu
//! ```

use std::io::{self, BufRead, Write};

use claw_agent_cli::{load_env, make_llm, make_memory};
use claw_core::agent::{Agent, AgentCommand, AgentId, ConvPhase, ConversationAgent, TickOutcome};
use owo_colors::{AnsiColors, OwoColorize};
use serde_json::Value;

const MEMORY_DIR: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../claw_core/output/conversation-chat");
const SYSTEM_PROMPT: &str = "You are a helpful, concise assistant.";
const AGENT_ID: usize = 1;

fn main() {
    load_env();

    let (memory, memory_view) = make_memory(AGENT_ID, MEMORY_DIR);
    // Tool-calling is required: the FSM is driven by the agent's control tools.
    let mut agent = ConversationAgent::builder(AgentId(AGENT_ID), make_llm(true), memory)
        .with_system_prompt(SYSTEM_PROMPT)
        .build()
        .expect("failed to build conversation agent");

    eprintln!("Memory: {MEMORY_DIR}");
    eprintln!("Type your message and press Enter. Empty line or Ctrl-D to quit.\n");

    let stdin = io::stdin();
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

        if input == "/messages" {
            let messages = memory_view.messages();
            println!(
                "{}",
                serde_json::to_string_pretty(&messages)
                    .unwrap_or_else(|e| format!("(serialize error: {e})"))
            );
            println!();
            continue;
        }

        // Remember where the transcript ends so we can show the tool calls this
        // turn appends, after the reply.
        let turn_start = message_count(&memory_view);

        if let Err(error) = agent.send_command(AgentCommand::AppendMessage(input.to_string())) {
            eprintln!("could not accept input: {error}");
            continue;
        }

        loop {
            match agent.tick() {
                TickOutcome::Working => continue,
                TickOutcome::Yielded { text } => {
                    println!("\n{text}");
                    break;
                }
                TickOutcome::Ended { final_message } => {
                    println!("\n{final_message}");
                    break;
                }
                TickOutcome::Failed(error) => {
                    eprintln!("agent error: {error}");
                    break;
                }
                TickOutcome::AwaitingApproval { id, summary } => {
                    // The Agent trait does not expose approval resolution, so this
                    // CLI cannot grant it; report and return to the prompt.
                    eprintln!(
                        "approval requested [{id}]: {summary} — not supported in this CLI; \
                         returning to prompt"
                    );
                    break;
                }
                TickOutcome::Cancelled { .. } | TickOutcome::Idle => break,
            }
        }

        print_tool_calls_since(&memory_view.messages(), turn_start);
        println!("{}", stage_label(agent.phase()));
        println!();
    }

    eprintln!("Goodbye.");
}

/// A colored `[stage: X]` label for the agent's current FSM stage. Each stage
/// gets its own color so progression is easy to track at a glance.
fn stage_label(phase: ConvPhase) -> String {
    let color = match phase {
        ConvPhase::Intake => AnsiColors::Blue,
        ConvPhase::Verify => AnsiColors::Yellow,
        ConvPhase::Delegate => AnsiColors::Magenta,
        ConvPhase::Watch => AnsiColors::Cyan,
        ConvPhase::Report => AnsiColors::Green,
    };
    format!("[stage: {phase}]").color(color).to_string()
}

/// Number of messages currently in the transcript.
fn message_count(memory: &claw_memory::ConversationMemory) -> usize {
    memory.messages().as_array().map_or(0, Vec::len)
}

/// Print, in gray under the model's reply, every tool call recorded after index
/// `start` — the calls the model made while producing this turn's answer.
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
