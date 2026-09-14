use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Event {
    pub sequence: u64,
    pub kind: String,
    pub payload: Value,
    pub emitted_at_ms: u128,
}

#[derive(Debug)]
pub struct EventBus {
    capacity: usize,
    next_sequence: u64,
    events: VecDeque<Event>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            next_sequence: 0,
            events: VecDeque::new(),
        }
    }

    pub fn emit(&mut self, kind: &str, payload: Value) -> Event {
        if let Some(previous) = self
            .events
            .back()
            .filter(|event| event.kind == kind && event.payload == payload)
        {
            return previous.clone();
        }
        self.next_sequence += 1;
        let event = Event {
            sequence: self.next_sequence,
            kind: kind.to_owned(),
            payload,
            emitted_at_ms: now_ms(),
        };
        self.events.push_back(event.clone());
        while self.events.len() > self.capacity {
            self.events.pop_front();
        }
        event
    }

    pub fn since(&self, sequence: u64, kind: Option<&str>) -> Vec<Event> {
        self.events
            .iter()
            .filter(|event| event.sequence > sequence)
            .filter(|event| kind.is_none_or(|expected| expected == event.kind))
            .cloned()
            .collect()
    }

    pub fn wait_for(&self, sequence: u64, kind: Option<&str>, timeout: Duration) -> Option<Event> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(event) = self.since(sequence, kind).into_iter().next() {
                return Some(event);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(5));
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
