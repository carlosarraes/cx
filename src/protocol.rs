use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub struct Turns {
    active: HashSet<String>,
    starting: HashMap<String, (String, bool)>,
}

impl Turns {
    pub fn starting(&mut self, id: &str, thread: &str) {
        self.starting
            .insert(id.to_owned(), (thread.to_owned(), false));
    }
    pub fn start_response(&mut self, id: &str, response: &Value) {
        if let Some((thread, observed)) = self.starting.remove(id) {
            if !observed && response.get("error").is_none() {
                // Block the gap when the response precedes turn/started.
                let thread = response
                    .pointer("/result/reviewThreadId")
                    .and_then(Value::as_str)
                    .unwrap_or(&thread);
                self.active.insert(thread.to_owned());
            }
        }
    }
    pub fn notification(&mut self, message: &Value) {
        let Some(thread) = message.pointer("/params/threadId").and_then(Value::as_str) else {
            return;
        };
        match message["method"].as_str() {
            Some("turn/started") => {
                self.active.insert(thread.to_owned());
                for (starting_thread, observed) in self.starting.values_mut() {
                    if starting_thread == thread {
                        *observed = true;
                    }
                }
            }
            Some("turn/completed") => {
                self.active.remove(thread);
            }
            _ => {}
        }
    }
    pub fn busy(&self) -> bool {
        !self.active.is_empty() || !self.starting.is_empty()
    }
}

pub fn starts_turn(method: &str) -> bool {
    matches!(
        method,
        "turn/start" | "review/start" | "thread/queue/start" | "thread/compact/start"
    )
}

#[derive(Default)]
pub struct Requests {
    serial: u64,
    originals: HashMap<String, (Value, String)>,
}
impl Requests {
    pub fn forward(&mut self, message: &mut Value) -> Option<String> {
        let method = message.get("method")?.as_str()?.to_owned();
        let original = message.get("id")?.clone();
        self.serial += 1;
        let id = format!("cx.tui.{}", self.serial);
        self.originals.insert(id.clone(), (original, method));
        message["id"] = Value::String(id.clone());
        Some(id)
    }
    pub fn restore(&mut self, message: &mut Value) -> Option<(String, String)> {
        if message.get("method").is_some() {
            return None;
        }
        let id = message.get("id")?.as_str()?.to_owned();
        let (original, method) = self.originals.remove(&id)?;
        message["id"] = original;
        Some((id, method))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_turn_start_request_blocks_switching_before_started_notification() {
        let mut turns = Turns::default();
        turns.starting("request-1", "a");
        assert!(turns.busy());
        turns.notification(
            &json!({"method":"turn/started", "params":{"threadId":"a", "turn":{"id":"1"}}}),
        );
        turns.start_response("request-1", &json!({"result":{}}));
        assert!(turns.busy());
        turns.notification(
            &json!({"method":"turn/completed", "params":{"threadId":"a", "turn":{"id":"1"}}}),
        );
        assert!(!turns.busy());
    }

    #[test]
    fn finishing_one_of_two_threads_does_not_make_the_runtime_idle() {
        let mut turns = Turns::default();
        for thread in ["a", "b"] {
            turns.notification(&json!({"method":"turn/started", "params":{"threadId":thread,"turn":{"id":thread}}}));
        }
        turns.notification(
            &json!({"method":"turn/completed", "params":{"threadId":"a","turn":{"id":"a"}}}),
        );
        assert!(turns.busy());
        turns.notification(
            &json!({"method":"turn/completed", "params":{"threadId":"b","turn":{"id":"b"}}}),
        );
        assert!(!turns.busy());
    }

    #[test]
    fn client_request_ids_cannot_collide_with_controller_ids() {
        let mut requests = Requests::default();
        let mut request = json!({"id":"cx.auth.1", "method":"thread/list","params":{}});
        let mapped = requests.forward(&mut request).unwrap();
        assert_ne!(mapped, "cx.auth.1");
        let mut response = json!({"id":mapped,"result":{"data":[]}});
        assert_eq!(requests.restore(&mut response).unwrap().1, "thread/list");
        assert_eq!(response["id"], "cx.auth.1");
        let mut unrelated = json!({"id":"cx.auth.1","result":{}});
        assert!(requests.restore(&mut unrelated).is_none());
    }
}
