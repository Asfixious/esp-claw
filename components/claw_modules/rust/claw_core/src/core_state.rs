//! Core request/response plumbing and control state.
//!
//! Ports the non-LLM parts of `claw_core_messages.c` and `claw_core_control.c`:
//! the request/response queues, the in-flight tracking, the user-interrupt
//! insert ring, the pending-response list, and the cancel/phase controls.
//!
//! Locking: `inflight` (a `std::sync::Mutex`) replaces the C `inflight_lock`;
//! `pending` replaces `response_lock` and also owns the out-of-order pending
//! response list. The bounded queues replace the FreeRTOS queues.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use claw_interfaces::error::{
    EspErr, ESP_ERR_INVALID_STATE, ESP_ERR_NOT_FOUND, ESP_ERR_NO_MEM, ESP_ERR_TIMEOUT,
};

use crate::channel::BoundedQueue;
use crate::consts::{
    AbortReason, AgentLoopPhase, INSERT_QUEUE_LEN, REQUEST_FLAG_USER_INTERRUPT,
};
use crate::error::CoreError;
use crate::request::RequestItem;
use crate::response::ResponseItem;

/// In-flight agent state, guarded by a single mutex (the C `inflight_lock`).
pub struct Inflight {
    pub request_id: u32,
    pub session_id: String,
    pub phase: AgentLoopPhase,
    pub abort: bool,
    pub abort_reason: AbortReason,
    pub insert_queue: VecDeque<RequestItem>,
}

impl Default for Inflight {
    fn default() -> Self {
        Inflight {
            request_id: 0,
            session_id: String::new(),
            phase: AgentLoopPhase::Idle,
            abort: false,
            abort_reason: AbortReason::None,
            insert_queue: VecDeque::with_capacity(INSERT_QUEUE_LEN),
        }
    }
}

/// Outcome of attempting to insert a user-interrupt request into the in-flight
/// turn. `Fallthrough` returns the item so the caller can enqueue it normally.
enum InsertOutcome {
    Inserted,
    Fallthrough(RequestItem),
    Reject(EspErr, RequestItem),
}

pub struct CoreState {
    pub initialized: AtomicBool,
    pub started: AtomicBool,
    pub stop_requested: AtomicBool,
    pub instance_id: u32,
    pub max_tool_iterations: u32,
    pub request_queue: BoundedQueue<RequestItem>,
    pub response_queue: BoundedQueue<ResponseItem>,
    pub inflight: Mutex<Inflight>,
    /// Replaces `response_lock`; also owns the out-of-order pending list.
    pub pending: Mutex<VecDeque<ResponseItem>>,
    /// Mirrors `inflight.abort` for the HTTP layer, which polls an `AtomicBool`
    /// without holding the inflight mutex.
    pub abort_flag: Arc<AtomicBool>,
}

impl CoreState {
    pub fn new(
        instance_id: u32,
        request_q_len: usize,
        response_q_len: usize,
        max_tool_iterations: u32,
    ) -> Self {
        CoreState {
            initialized: AtomicBool::new(true),
            started: AtomicBool::new(false),
            stop_requested: AtomicBool::new(false),
            instance_id,
            max_tool_iterations,
            request_queue: BoundedQueue::new(request_q_len),
            response_queue: BoundedQueue::new(response_q_len),
            inflight: Mutex::new(Inflight::default()),
            pending: Mutex::new(VecDeque::new()),
            abort_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    // --- ingress (claw_core_messages.c) -----------------------------------

    /// `claw_core_ingress_submit`
    pub fn ingress_submit(&self, item: RequestItem, timeout_ms: u32) -> Result<(), CoreError> {
        if !self.started.load(Ordering::Acquire) {
            return Err(CoreError::InvalidState);
        }
        if item.user_text.is_empty() {
            return Err(CoreError::InvalidArg);
        }

        let item = if item.flags & REQUEST_FLAG_USER_INTERRUPT != 0 {
            match self.try_queue_insert(item) {
                InsertOutcome::Inserted => return Ok(()),
                InsertOutcome::Reject(err, _item) => return Err(CoreError::Esp(err)),
                InsertOutcome::Fallthrough(item) => item,
            }
        } else {
            item
        };

        let timeout = timeout_to_duration(timeout_ms);
        match self.request_queue.send_timeout(item, timeout) {
            Ok(()) => Ok(()),
            Err(_) => Err(CoreError::Esp(ESP_ERR_TIMEOUT)),
        }
    }

    fn try_queue_insert(&self, item: RequestItem) -> InsertOutcome {
        let mut inf = self.inflight.lock().unwrap();

        let session_ok = !item.session_id_str().is_empty()
            && inf.request_id != 0
            && !inf.session_id.is_empty()
            && inf.session_id == item.session_id_str();
        if !session_ok {
            return InsertOutcome::Fallthrough(item); // ESP_ERR_NOT_FOUND
        }
        if !inf.phase.is_insertable() {
            return InsertOutcome::Fallthrough(item); // ESP_ERR_INVALID_STATE
        }
        if inf.insert_queue.len() >= INSERT_QUEUE_LEN {
            return InsertOutcome::Reject(ESP_ERR_NO_MEM, item);
        }

        inf.insert_queue.push_back(item);
        if inf.phase == AgentLoopPhase::InLlmHttp && inf.abort_reason != AbortReason::Cancel {
            inf.abort = true;
            inf.abort_reason = AbortReason::UserInterrupt;
            self.abort_flag.store(true, Ordering::Release);
        }
        InsertOutcome::Inserted
    }

    /// `claw_core_ingress_dequeue_inserted_user_inputs`
    pub fn dequeue_inserted_user_inputs(&self, session_id: &str, max: usize) -> Vec<String> {
        let mut out = Vec::new();
        if session_id.is_empty() || max == 0 {
            return out;
        }
        let mut inf = self.inflight.lock().unwrap();
        while out.len() < max {
            match inf.insert_queue.front() {
                Some(front) if front.session_id_str() == session_id => {
                    let item = inf.insert_queue.pop_front().unwrap();
                    out.push(item.user_text);
                }
                _ => break,
            }
        }
        out
    }

    pub fn clear_insert_queue(&self) {
        let mut inf = self.inflight.lock().unwrap();
        inf.insert_queue.clear();
    }

    // --- response path (claw_core_messages.c) -----------------------------

    /// `claw_core_response_push`
    pub fn response_push(&self, item: ResponseItem) {
        // portMAX_DELAY in C; block until space.
        let _ = self.response_queue.send_timeout(item, None);
    }

    /// `claw_core_response_receive_for`
    pub fn response_receive_for(
        &self,
        request_id: u32,
        timeout_ms: u32,
    ) -> Result<ResponseItem, EspErr> {
        if !self.started.load(Ordering::Acquire) {
            return Err(ESP_ERR_INVALID_STATE);
        }
        let match_any = request_id == 0;
        // The pending lock serializes receivers (the C response_lock).
        let mut pending = self.pending.lock().unwrap();

        if let Some(idx) = pending
            .iter()
            .position(|r| match_any || r.request_id == request_id)
        {
            return Ok(pending.remove(idx).unwrap());
        }

        let start = Instant::now();
        loop {
            let wait = if timeout_ms == u32::MAX {
                None
            } else {
                let total = Duration::from_millis(timeout_ms as u64);
                let elapsed = start.elapsed();
                if elapsed >= total {
                    return Err(ESP_ERR_TIMEOUT);
                }
                Some(total - elapsed)
            };
            match self.response_queue.recv_timeout(wait) {
                None => return Err(ESP_ERR_TIMEOUT),
                Some(item) => {
                    if match_any || item.request_id == request_id {
                        return Ok(item);
                    }
                    pending.push_back(item);
                }
            }
        }
    }

    // --- control (claw_core_control.c) ------------------------------------

    /// `claw_core_control_set_phase`
    pub fn set_phase(&self, phase: AgentLoopPhase) {
        let mut inf = self.inflight.lock().unwrap();
        inf.phase = phase;
    }

    /// `claw_core_control_get_phase`
    pub fn get_phase(&self) -> AgentLoopPhase {
        if !self.initialized.load(Ordering::Acquire) {
            return AgentLoopPhase::Idle;
        }
        self.inflight.lock().unwrap().phase
    }

    /// `claw_core_control_cancel_request`
    pub fn cancel_request(&self, request_id: u32) -> Result<(), CoreError> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err(CoreError::InvalidState);
        }
        let mut inf = self.inflight.lock().unwrap();
        if inf.request_id != 0 && (request_id == 0 || inf.request_id == request_id) {
            inf.abort = true;
            inf.abort_reason = AbortReason::Cancel;
            self.abort_flag.store(true, Ordering::Release);
            Ok(())
        } else {
            Err(CoreError::Esp(ESP_ERR_NOT_FOUND))
        }
    }

    /// `claw_core_control_take_user_interrupt_http_abort`
    pub fn take_user_interrupt_http_abort(&self, request_id: u32) -> bool {
        let mut inf = self.inflight.lock().unwrap();
        if inf.request_id == request_id
            && inf.abort
            && inf.abort_reason == AbortReason::UserInterrupt
        {
            inf.abort = false;
            inf.abort_reason = AbortReason::None;
            self.abort_flag.store(false, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// `claw_core_control_clear_user_interrupt_abort`
    pub fn clear_user_interrupt_abort(&self, request_id: u32) {
        let mut inf = self.inflight.lock().unwrap();
        if inf.request_id == request_id && inf.abort_reason == AbortReason::UserInterrupt {
            inf.abort = false;
            inf.abort_reason = AbortReason::None;
            self.abort_flag.store(false, Ordering::Release);
        }
    }

    /// Begin a turn: record the in-flight request id/session, reset abort, set
    /// the initial phase, and clear any stale inserts (mirrors the start-of-turn
    /// block in `claw_core_agent_loop_task`).
    pub fn begin_turn(&self, request_id: u32, session_id: &str) {
        let mut inf = self.inflight.lock().unwrap();
        inf.request_id = request_id;
        inf.session_id.clear();
        // mirror the 128-byte bound from CLAW_CORE_INFLIGHT_SESSION_ID_SIZE
        let bytes = session_id.as_bytes();
        let take = bytes.len().min(crate::consts::INFLIGHT_SESSION_ID_SIZE - 1);
        inf.session_id.push_str(&session_id[..floor_char_boundary(session_id, take)]);
        inf.phase = AgentLoopPhase::BeforeBuildIterationContext;
        inf.abort = false;
        inf.abort_reason = AbortReason::None;
        inf.insert_queue.clear();
        self.abort_flag.store(false, Ordering::Release);
    }

    /// End a turn: clear the in-flight request and any leftover inserts. Returns
    /// whether the turn was cancelled (abort armed with `Cancel`), so the loop
    /// can replace the transport error with "request cancelled".
    pub fn end_turn(&self) -> bool {
        let mut inf = self.inflight.lock().unwrap();
        let was_cancelled = inf.abort && inf.abort_reason == AbortReason::Cancel;
        inf.request_id = 0;
        inf.session_id.clear();
        inf.phase = AgentLoopPhase::Idle;
        inf.abort = false;
        inf.abort_reason = AbortReason::None;
        inf.insert_queue.clear();
        self.abort_flag.store(false, Ordering::Release);
        was_cancelled
    }
}

fn timeout_to_duration(timeout_ms: u32) -> Option<Duration> {
    if timeout_ms == u32::MAX {
        None
    } else {
        Some(Duration::from_millis(timeout_ms as u64))
    }
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consts::REQUEST_FLAG_USER_INTERRUPT;

    fn req(id: u32, session: &str, text: &str, flags: u32) -> RequestItem {
        RequestItem {
            request_id: id,
            flags,
            session_id: Some(session.to_string()),
            user_text: text.to_string(),
            ..Default::default()
        }
    }

    fn core() -> CoreState {
        let c = CoreState::new(1, 4, 4, 10);
        c.started.store(true, Ordering::Release);
        c
    }

    #[test]
    fn submit_rejects_when_not_started() {
        let c = CoreState::new(1, 4, 4, 10);
        assert!(matches!(
            c.ingress_submit(req(1, "s", "hi", 0), 0).unwrap_err(),
            CoreError::InvalidState
        ));
    }

    #[test]
    fn submit_rejects_empty_text() {
        let c = core();
        assert!(matches!(
            c.ingress_submit(req(1, "s", "", 0), 0).unwrap_err(),
            CoreError::InvalidArg
        ));
    }

    #[test]
    fn submit_and_receive_roundtrip() {
        let c = core();
        c.ingress_submit(req(7, "s", "hi", 0), 1000).unwrap();
        // simulate the agent consuming the request and pushing a response
        let got = c.request_queue.recv_timeout(Some(Duration::from_millis(50))).unwrap();
        assert_eq!(got.request_id, 7);
        c.response_push(ResponseItem { request_id: 7, text: Some("ok".into()), ..Default::default() });
        let resp = c.response_receive_for(7, 1000).unwrap();
        assert_eq!(resp.request_id, 7);
        assert_eq!(resp.text.as_deref(), Some("ok"));
    }

    #[test]
    fn receive_out_of_order_uses_pending() {
        let c = core();
        c.response_push(ResponseItem { request_id: 1, text: Some("a".into()), ..Default::default() });
        c.response_push(ResponseItem { request_id: 2, text: Some("b".into()), ..Default::default() });
        // ask for id 2 first; id 1 should be parked in pending and returned next
        let r2 = c.response_receive_for(2, 1000).unwrap();
        assert_eq!(r2.request_id, 2);
        let r1 = c.response_receive_for(1, 1000).unwrap();
        assert_eq!(r1.request_id, 1);
    }

    #[test]
    fn receive_timeout() {
        let c = core();
        assert_eq!(c.response_receive_for(0, 10).err(), Some(ESP_ERR_TIMEOUT));
    }

    #[test]
    fn cancel_and_phase() {
        let c = core();
        assert!(matches!(
            c.cancel_request(0).unwrap_err(),
            CoreError::Esp(ESP_ERR_NOT_FOUND)
        ));
        c.begin_turn(5, "s");
        c.cancel_request(5).unwrap();
        assert!(c.abort_flag.load(Ordering::Acquire));
        assert!(matches!(
            c.cancel_request(6).unwrap_err(),
            CoreError::Esp(ESP_ERR_NOT_FOUND)
        ));
        c.set_phase(AgentLoopPhase::InLlmHttp);
        assert_eq!(c.get_phase(), AgentLoopPhase::InLlmHttp);
    }

    #[test]
    fn user_interrupt_insert_during_llm_http() {
        let c = core();
        c.begin_turn(5, "sess");
        c.set_phase(AgentLoopPhase::InLlmHttp);
        // interrupt for the same session is inserted and arms abort
        c.ingress_submit(req(6, "sess", "more", REQUEST_FLAG_USER_INTERRUPT), 0)
            .unwrap();
        assert!(c.abort_flag.load(Ordering::Acquire));
        let inserted = c.dequeue_inserted_user_inputs("sess", 4);
        assert_eq!(inserted, vec!["more".to_string()]);
    }

    #[test]
    fn user_interrupt_falls_through_when_no_match() {
        let c = core();
        // no in-flight turn -> falls through to the normal request queue
        c.ingress_submit(req(6, "sess", "more", REQUEST_FLAG_USER_INTERRUPT), 1000)
            .unwrap();
        let got = c.request_queue.recv_timeout(Some(Duration::from_millis(50))).unwrap();
        assert_eq!(got.request_id, 6);
    }
}
