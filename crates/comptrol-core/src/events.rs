use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Event {
    pub sequence: u64,
    pub kind: String,
    pub payload: Value,
    pub emitted_at_ms: u128,
}

#[derive(Debug)]
struct EventState {
    capacity: usize,
    next_sequence: u64,
    events: VecDeque<Event>,
}

#[derive(Debug)]
pub struct EventBus {
    state: Mutex<EventState>,
    notification: Condvar,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::new(EventState {
                capacity: capacity.max(1),
                next_sequence: 0,
                events: VecDeque::new(),
            }),
            notification: Condvar::new(),
        }
    }

    pub fn emit(&self, kind: &str, payload: Value) -> Event {
        let mut state = self.state.lock().expect("event bus state lock");
        if let Some(previous) = state
            .events
            .back()
            .filter(|event| event.kind == kind && event.payload == payload)
        {
            return previous.clone();
        }
        state.next_sequence += 1;
        let event = Event {
            sequence: state.next_sequence,
            kind: kind.to_owned(),
            payload,
            emitted_at_ms: now_ms(),
        };
        state.events.push_back(event.clone());
        while state.events.len() > state.capacity {
            state.events.pop_front();
        }
        self.notification.notify_all();
        event
    }

    pub fn since(&self, sequence: u64, kind: Option<&str>) -> Vec<Event> {
        let state = self.state.lock().expect("event bus state lock");
        state
            .events
            .iter()
            .filter(|event| event.sequence > sequence)
            .filter(|event| kind.is_none_or(|expected| expected == event.kind))
            .cloned()
            .collect()
    }

    pub fn wait_for(&self, sequence: u64, kind: Option<&str>, timeout: Duration) -> Option<Event> {
        let mut state = self.state.lock().ok()?;
        if let Some(event) = state
            .events
            .iter()
            .find(|event| {
                event.sequence > sequence && kind.is_none_or(|expected| expected == event.kind)
            })
            .cloned()
        {
            return Some(event);
        }
        if timeout.is_zero() {
            return None;
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(event) = state
                .events
                .iter()
                .find(|event| {
                    event.sequence > sequence && kind.is_none_or(|expected| expected == event.kind)
                })
                .cloned()
            {
                return Some(event);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let (next, result) = self.notification.wait_timeout(state, remaining).ok()?;
            state = next;
            if result.timed_out() {
                return state
                    .events
                    .iter()
                    .find(|event| {
                        event.sequence > sequence
                            && kind.is_none_or(|expected| expected == event.kind)
                    })
                    .cloned();
            }
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
