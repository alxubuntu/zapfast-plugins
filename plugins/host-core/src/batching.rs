//! Debouncing incoming message identifiers without holding message bodies.
//!
//! The caller decides whether a chat is eligible when scheduling. Reading a
//! chat afterwards does not cancel its pending work; explicit opt-out does.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Pending message identifiers for one account. Keep one queue per account.
#[derive(Default)]
pub struct Pending {
    chats: HashMap<String, Batch>,
}

struct Batch {
    deadline: Instant,
    messages: Vec<String>,
}

/// A due batch. Its owner must recheck authorization before reading history.
pub struct Due {
    pub chat: String,
    pub messages: Vec<String>,
}

impl Pending {
    pub fn next_deadline(&self) -> Option<Instant> {
        self.chats.values().map(|batch| batch.deadline).min()
    }

    /// Schedule a new message, restarting the wait for its chat.
    pub fn push(&mut self, chat: &str, message: &str, now: Instant, wait: Duration) {
        let batch = self.chats.entry(chat.to_owned()).or_insert_with(|| Batch {
            deadline: now + wait,
            messages: Vec::new(),
        });
        if !batch.messages.iter().any(|id| id == message) {
            batch.messages.push(message.to_owned());
            batch.deadline = now + wait;
        }
    }

    /// Cancel work on explicit chat opt-out.
    pub fn cancel(&mut self, chat: &str) {
        self.chats.remove(chat);
    }

    /// Cancel all work on global opt-out, logout or account removal.
    pub fn clear(&mut self) {
        self.chats.clear();
    }

    /// Remove all batches whose quiet period has elapsed.
    pub fn take_due(&mut self, now: Instant) -> Vec<Due> {
        let mut due = Vec::new();
        self.chats.retain(|chat, batch| {
            if batch.deadline > now {
                return true;
            }
            due.push(Due {
                chat: chat.clone(),
                messages: std::mem::take(&mut batch.messages),
            });
            false
        });
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_messages_after_last_arrival_without_extending_on_duplicates() {
        let now = Instant::now();
        let wait = Duration::from_secs(30);
        let mut pending = Pending::default();
        pending.push("a", "one", now, wait);
        pending.push("a", "two", now + Duration::from_secs(10), wait);
        pending.push("a", "two", now + Duration::from_secs(20), wait);
        assert!(pending.take_due(now + Duration::from_secs(39)).is_empty());
        let due = pending.take_due(now + Duration::from_secs(40));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].chat, "a");
        assert_eq!(due[0].messages, ["one", "two"]);
        assert!(pending.take_due(now + Duration::from_secs(41)).is_empty());
    }

    #[test]
    fn explicit_opt_out_isolated_by_chat_and_account() {
        let now = Instant::now();
        let mut first = Pending::default();
        let mut second = Pending::default();
        first.push("a", "one", now, Duration::ZERO);
        first.push("b", "two", now, Duration::ZERO);
        second.push("a", "three", now, Duration::ZERO);
        first.cancel("a");
        let due = first.take_due(now);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].chat, "b");
        assert_eq!(second.take_due(now)[0].messages, ["three"]);
        first.push("a", "four", now, Duration::ZERO);
        first.clear();
        assert!(first.take_due(now).is_empty());
    }
}
