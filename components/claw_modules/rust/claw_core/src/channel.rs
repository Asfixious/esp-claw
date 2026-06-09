//! A small std-only bounded blocking queue replacing the FreeRTOS queues
//! (`xQueueCreate`/`xQueueSend`/`xQueueReceive`) used for the request and
//! response paths. A `None` timeout means block forever (`portMAX_DELAY`).

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub struct BoundedQueue<T> {
    inner: Mutex<VecDeque<T>>,
    not_full: Condvar,
    not_empty: Condvar,
    cap: usize,
}

impl<T> BoundedQueue<T> {
    pub fn new(cap: usize) -> Self {
        BoundedQueue {
            inner: Mutex::new(VecDeque::with_capacity(cap)),
            not_full: Condvar::new(),
            not_empty: Condvar::new(),
            cap: cap.max(1),
        }
    }

    /// Enqueue, blocking until space is available or the timeout elapses.
    /// Returns the item back on timeout (mirrors `xQueueSend` returning errd).
    pub fn send_timeout(&self, item: T, timeout: Option<Duration>) -> Result<(), T> {
        let mut guard = self.inner.lock().unwrap();
        match timeout {
            None => {
                while guard.len() >= self.cap {
                    guard = self.not_full.wait(guard).unwrap();
                }
            }
            Some(dur) => {
                let deadline = Instant::now() + dur;
                while guard.len() >= self.cap {
                    let now = Instant::now();
                    if now >= deadline {
                        return Err(item);
                    }
                    let (g, res) = self.not_full.wait_timeout(guard, deadline - now).unwrap();
                    guard = g;
                    if res.timed_out() && guard.len() >= self.cap {
                        return Err(item);
                    }
                }
            }
        }
        guard.push_back(item);
        drop(guard);
        self.not_empty.notify_one();
        Ok(())
    }

    /// Dequeue, blocking until an item is available or the timeout elapses.
    pub fn recv_timeout(&self, timeout: Option<Duration>) -> Option<T> {
        let mut guard = self.inner.lock().unwrap();
        match timeout {
            None => {
                while guard.is_empty() {
                    guard = self.not_empty.wait(guard).unwrap();
                }
            }
            Some(dur) => {
                let deadline = Instant::now() + dur;
                while guard.is_empty() {
                    let now = Instant::now();
                    if now >= deadline {
                        return None;
                    }
                    let (g, res) = self.not_empty.wait_timeout(guard, deadline - now).unwrap();
                    guard = g;
                    if res.timed_out() && guard.is_empty() {
                        return None;
                    }
                }
            }
        }
        let item = guard.pop_front();
        drop(guard);
        self.not_full.notify_one();
        item
    }

    /// Non-blocking drain of all queued items (used during shutdown cleanup).
    pub fn drain(&self) -> Vec<T> {
        let mut guard = self.inner.lock().unwrap();
        let out: Vec<T> = guard.drain(..).collect();
        drop(guard);
        self.not_full.notify_all();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn send_recv_roundtrip() {
        let q = BoundedQueue::new(2);
        assert!(q.send_timeout(1u32, Some(Duration::from_millis(10))).is_ok());
        assert!(q.send_timeout(2u32, Some(Duration::from_millis(10))).is_ok());
        // full now
        assert_eq!(q.send_timeout(3u32, Some(Duration::from_millis(10))), Err(3));
        assert_eq!(q.recv_timeout(Some(Duration::from_millis(10))), Some(1));
        assert_eq!(q.recv_timeout(Some(Duration::from_millis(10))), Some(2));
        assert_eq!(q.recv_timeout(Some(Duration::from_millis(10))), None);
    }

    #[test]
    fn blocking_send_wakes_on_recv() {
        let q = Arc::new(BoundedQueue::new(1));
        q.send_timeout(1u32, None).unwrap();
        let q2 = q.clone();
        let producer = thread::spawn(move || {
            // blocks until consumer frees a slot
            q2.send_timeout(2u32, None).unwrap();
        });
        assert_eq!(q.recv_timeout(None), Some(1));
        producer.join().unwrap();
        assert_eq!(q.recv_timeout(None), Some(2));
    }
}
