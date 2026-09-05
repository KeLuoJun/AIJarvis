use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

const RATIO_SCALE: usize = 1_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatRequestStatus {
    Accepted,
    Replied,
    Failed,
    Cancelled,
}

impl ChatRequestStatus {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Replied | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ChatRequestId(pub u64);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatAccepted {
    pub request_id: ChatRequestId,
    pub status: ChatRequestStatus,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ChatFailureKind {
    ModelUnavailable,
    Timeout,
    InvalidResponse,
    QueueFull,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatStatusEvent {
    pub request_id: ChatRequestId,
    pub runtime_generation: u64,
    pub status: ChatRequestStatus,
    pub user_message: String,
    pub reply: Option<String>,
    pub failure: Option<ChatFailureKind>,
}

impl ChatStatusEvent {
    pub fn validate(&self) -> bool {
        match self.status {
            ChatRequestStatus::Accepted => self.reply.is_none() && self.failure.is_none(),
            ChatRequestStatus::Replied => {
                self.reply
                    .as_ref()
                    .is_some_and(|reply| !reply.trim().is_empty())
                    && self.failure.is_none()
            }
            ChatRequestStatus::Failed => self.reply.is_none() && self.failure.is_some(),
            ChatRequestStatus::Cancelled => self.reply.is_none() && self.failure.is_none(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatTurn {
    pub user: String,
    pub assistant: String,
    pub user_tokens: usize,
    pub assistant_tokens: usize,
}

impl ChatTurn {
    pub const fn token_count(&self) -> usize {
        self.user_tokens.saturating_add(self.assistant_tokens)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChatHistoryLimits {
    pub available_tokens: usize,
    pub evict_at_per_mille: usize,
    pub evict_to_per_mille: usize,
}

impl ChatHistoryLimits {
    pub fn validate(self) -> Option<Self> {
        (self.available_tokens > 0
            && self.evict_to_per_mille < self.evict_at_per_mille
            && self.evict_at_per_mille <= RATIO_SCALE)
            .then_some(self)
    }

    const fn tokens_at(self, ratio: usize) -> usize {
        self.available_tokens.saturating_mul(ratio) / RATIO_SCALE
    }
}

#[derive(Clone, Debug)]
pub struct ChatHistory {
    limits: ChatHistoryLimits,
    turns: VecDeque<ChatTurn>,
    token_count: usize,
}

impl ChatHistory {
    pub fn new(limits: ChatHistoryLimits) -> Option<Self> {
        Some(Self {
            limits: limits.validate()?,
            turns: VecDeque::new(),
            token_count: 0,
        })
    }

    pub fn prepare_for(&mut self, incoming_tokens: usize) {
        if self.token_count.saturating_add(incoming_tokens)
            < self.limits.tokens_at(self.limits.evict_at_per_mille)
        {
            return;
        }
        let target = self.limits.tokens_at(self.limits.evict_to_per_mille);
        while self.token_count.saturating_add(incoming_tokens) > target {
            let Some(turn) = self.turns.pop_front() else {
                break;
            };
            self.token_count = self.token_count.saturating_sub(turn.token_count());
        }
    }

    pub fn push_completed(&mut self, turn: ChatTurn) {
        self.token_count = self.token_count.saturating_add(turn.token_count());
        self.turns.push_back(turn);
        self.prepare_for(0);
    }

    pub fn turns(&self) -> impl ExactSizeIterator<Item = &ChatTurn> {
        self.turns.iter()
    }

    pub const fn token_count(&self) -> usize {
        self.token_count
    }

    pub fn clear(&mut self) {
        self.turns.clear();
        self.token_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> ChatHistory {
        ChatHistory::new(ChatHistoryLimits {
            available_tokens: 100,
            evict_at_per_mille: 800,
            evict_to_per_mille: 700,
        })
        .expect("valid limits")
    }

    fn turn(name: &str, tokens: usize) -> ChatTurn {
        ChatTurn {
            user: format!("user-{name}"),
            assistant: format!("assistant-{name}"),
            user_tokens: tokens / 2,
            assistant_tokens: tokens - tokens / 2,
        }
    }

    #[test]
    fn status_contract_has_exactly_three_terminal_states() {
        assert!(!ChatRequestStatus::Accepted.is_terminal());
        for status in [
            ChatRequestStatus::Replied,
            ChatRequestStatus::Failed,
            ChatRequestStatus::Cancelled,
        ] {
            assert!(status.is_terminal());
        }
    }

    #[test]
    fn event_shape_rejects_ambiguous_terminal_payloads() {
        let mut event = ChatStatusEvent {
            request_id: ChatRequestId(1),
            runtime_generation: 2,
            status: ChatRequestStatus::Replied,
            user_message: "hello".into(),
            reply: Some("world".into()),
            failure: None,
        };
        assert!(event.validate());
        event.failure = Some(ChatFailureKind::Internal);
        assert!(!event.validate());
        event.status = ChatRequestStatus::Failed;
        event.reply = None;
        assert!(event.validate());
    }

    #[test]
    fn eviction_removes_only_complete_oldest_turns_to_the_target() {
        let mut history = history();
        history.push_completed(turn("one", 30));
        history.push_completed(turn("two", 30));
        assert_eq!(history.token_count(), 60);

        history.prepare_for(20);

        let remaining = history.turns().collect::<Vec<_>>();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].user, "user-two");
        assert_eq!(history.token_count(), 30);
    }

    #[test]
    fn completed_reply_can_trigger_the_same_whole_turn_eviction() {
        let mut history = history();
        history.push_completed(turn("one", 40));
        history.push_completed(turn("two", 40));
        assert_eq!(history.turns().len(), 1);
        assert_eq!(history.turns().next().expect("newest").user, "user-two");
    }

    #[test]
    fn oversized_single_turn_is_evicted_as_one_complete_turn() {
        let mut history = history();
        history.push_completed(turn("large", 90));
        assert_eq!(history.turns().len(), 0);
        assert_eq!(history.token_count(), 0);
    }

    #[test]
    fn clear_removes_all_session_context() {
        let mut history = history();
        history.push_completed(turn("one", 20));
        history.clear();
        assert_eq!(history.turns().len(), 0);
        assert_eq!(history.token_count(), 0);
    }

    #[test]
    fn invalid_thresholds_are_rejected() {
        assert!(ChatHistory::new(ChatHistoryLimits {
            available_tokens: 100,
            evict_at_per_mille: 700,
            evict_to_per_mille: 800,
        })
        .is_none());
    }
}
