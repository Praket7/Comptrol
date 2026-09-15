use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, mpsc};
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
    subscribers: Mutex<Vec<mpsc::Sender<Event>>>,
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
            subscribers: Mutex::new(Vec::new()),
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
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.retain(|subscriber| subscriber.send(event.clone()).is_ok());
        }
        self.notification.notify_all();
        event
    }

    /// Subscribe without coupling producers to a consumer's processing speed.
    /// The channel is unbounded and delivery is best-effort; replay remains
    /// available through `snapshot_since` for reconnecting consumers.
    pub fn subscribe(&self) -> mpsc::Receiver<Event> {
        let (sender, receiver) = mpsc::channel();
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.push(sender);
        }
        receiver
    }

    pub fn snapshot_since(&self, sequence: u64, kind: Option<&str>) -> Vec<Event> {
        self.since(sequence, kind)
    }

    pub fn latest_sequence(&self) -> u64 {
        self.state
            .lock()
            .map(|state| state.next_sequence)
            .unwrap_or_default()
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn subscription_is_non_blocking_and_replay_is_bounded() {
        let bus = EventBus::new(2);
        let receiver = bus.subscribe();
        bus.emit("one", json!({"n": 1}));
        bus.emit("two", json!({"n": 2}));
        bus.emit("three", json!({"n": 3}));
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_millis(20))
                .unwrap()
                .kind,
            "one"
        );
        assert_eq!(bus.latest_sequence(), 3);
        assert_eq!(bus.snapshot_since(0, None).len(), 2);
        assert_eq!(bus.snapshot_since(1, Some("three")).len(), 1);
    }
}
