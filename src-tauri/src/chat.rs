use std::{
    collections::{BTreeMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex as AsyncMutex};

use crate::{
    error::AppError,
    llama::{HttpTransport, LlamaError, LlamaService, ModelOutput, ModelRequest, ProcessSpawner},
    prompts::PromptId,
    state::{ChatRuntimeConfig, RuntimeState},
    supervisor::SupervisorError,
    types::WorkScope,
};

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

#[derive(Clone, Debug)]
pub struct ChatModelInput {
    pub persona: String,
    pub runtime_state: Value,
    pub history: Vec<ChatTurn>,
    pub user_message: String,
}

pub type ChatModelFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ChatFailureKind>> + Send + 'a>>;

pub trait ChatModel: Send + Sync {
    fn tokenize<'a>(&'a self, content: &'a str) -> ChatModelFuture<'a, usize>;
    fn reply<'a>(&'a self, input: ChatModelInput) -> ChatModelFuture<'a, String>;
}

pub trait ChatEventSink: Send + Sync {
    fn emit(&self, event: ChatStatusEvent);
}

pub struct UnavailableChatModel;

impl ChatModel for UnavailableChatModel {
    fn tokenize<'a>(&'a self, _content: &'a str) -> ChatModelFuture<'a, usize> {
        Box::pin(async { Err(ChatFailureKind::ModelUnavailable) })
    }

    fn reply<'a>(&'a self, _input: ChatModelInput) -> ChatModelFuture<'a, String> {
        Box::pin(async { Err(ChatFailureKind::ModelUnavailable) })
    }
}

pub struct LlamaChatModel<S, T> {
    service: Arc<LlamaService<S, T>>,
    tokenize_timeout: std::time::Duration,
    model_timeout: std::time::Duration,
}

impl<S, T> LlamaChatModel<S, T> {
    pub fn new(service: Arc<LlamaService<S, T>>, config: ChatRuntimeConfig) -> Self {
        Self {
            service,
            tokenize_timeout: config.tokenize_timeout,
            model_timeout: config.model_timeout,
        }
    }
}

impl<S, T> ChatModel for LlamaChatModel<S, T>
where
    S: ProcessSpawner + 'static,
    T: HttpTransport + 'static,
{
    fn tokenize<'a>(&'a self, content: &'a str) -> ChatModelFuture<'a, usize> {
        Box::pin(async move {
            self.service
                .tokenize(content, self.tokenize_timeout)
                .await
                .map_err(map_llama_error)
        })
    }

    fn reply<'a>(&'a self, input: ChatModelInput) -> ChatModelFuture<'a, String> {
        Box::pin(async move {
            let variables = BTreeMap::from([
                ("persona".into(), Value::String(input.persona)),
                (
                    "runtime_state".into(),
                    Value::String(input.runtime_state.to_string()),
                ),
                (
                    "conversation_history".into(),
                    Value::String(
                        serde_json::to_string(&input.history)
                            .map_err(|_| ChatFailureKind::Internal)?,
                    ),
                ),
                ("user_message".into(), Value::String(input.user_message)),
            ]);
            match self
                .service
                .request(
                    ModelRequest {
                        prompt: PromptId::DirectChat,
                        variables,
                        image_data_urls: Vec::new(),
                    },
                    self.model_timeout,
                )
                .await
                .map_err(map_llama_error)?
            {
                ModelOutput::DirectChat(output) => Ok(output.message),
                ModelOutput::Observation(_) => Err(ChatFailureKind::InvalidResponse),
            }
        })
    }
}

fn map_llama_error(error: LlamaError) -> ChatFailureKind {
    match error {
        LlamaError::ArtifactMissing
        | LlamaError::Spawn
        | LlamaError::AlreadyStarted
        | LlamaError::NotReady
        | LlamaError::ProcessExited => ChatFailureKind::ModelUnavailable,
        LlamaError::Timeout => ChatFailureKind::Timeout,
        LlamaError::Protocol | LlamaError::Schema => ChatFailureKind::InvalidResponse,
        LlamaError::Config | LlamaError::Http | LlamaError::Stopping => ChatFailureKind::Internal,
    }
}

pub struct ChatEngine {
    runtime: Arc<RuntimeState>,
    model: Arc<dyn ChatModel>,
    sink: Arc<dyn ChatEventSink>,
    persona: String,
    config: ChatRuntimeConfig,
    history: Mutex<ChatHistory>,
    turn_lock: AsyncMutex<()>,
    next_request_id: AtomicU64,
}

impl ChatEngine {
    pub fn new(
        runtime: Arc<RuntimeState>,
        model: Arc<dyn ChatModel>,
        sink: Arc<dyn ChatEventSink>,
        persona: String,
        config: ChatRuntimeConfig,
    ) -> Result<Arc<Self>, AppError> {
        if persona.trim().is_empty() {
            return Err(AppError::invalid_argument("The active persona is empty."));
        }
        Self::build(runtime, model, sink, persona, config)
    }

    pub fn unavailable(
        runtime: Arc<RuntimeState>,
        sink: Arc<dyn ChatEventSink>,
        config: ChatRuntimeConfig,
    ) -> Result<Arc<Self>, AppError> {
        Self::build(
            runtime,
            Arc::new(UnavailableChatModel),
            sink,
            String::new(),
            config,
        )
    }

    fn build(
        runtime: Arc<RuntimeState>,
        model: Arc<dyn ChatModel>,
        sink: Arc<dyn ChatEventSink>,
        persona: String,
        config: ChatRuntimeConfig,
    ) -> Result<Arc<Self>, AppError> {
        let history = ChatHistory::new(config.history_limits)
            .ok_or_else(|| AppError::invalid_argument("Chat history limits are invalid."))?;
        Ok(Arc::new(Self {
            runtime,
            model,
            sink,
            persona,
            config,
            history: Mutex::new(history),
            turn_lock: AsyncMutex::new(()),
            next_request_id: AtomicU64::new(0),
        }))
    }

    pub fn submit_text(self: &Arc<Self>, message: String) -> Result<ChatAccepted, AppError> {
        let message = message.trim().to_owned();
        if message.is_empty() || message.chars().count() > self.config.max_message_chars {
            return Err(AppError::invalid_argument(
                "The message is empty or exceeds the configured limit.",
            ));
        }
        let ticket = self.runtime.admit(WorkScope::Direct)?;
        let request_id = self.next_request_id()?;
        let generation = ticket.global_generation();
        let cancellation = ticket.cancellation_token();
        let engine = Arc::clone(self);
        let events = Arc::new(RequestEvents::new(
            request_id,
            generation,
            message.clone(),
            Arc::clone(&self.sink),
        ));
        let task_events = Arc::clone(&events);
        let cancel_events = Arc::clone(&events);
        let task_message = message.clone();
        self.runtime
            .supervisor()
            .try_spawn_direct(
                move |supervisor_cancel| async move {
                    let terminal = TerminalGuard::new(task_events);
                    let outcome = tokio::select! {
                        _ = cancellation.cancelled() => TurnOutcome::Cancelled,
                        _ = supervisor_cancel.cancelled() => TurnOutcome::Cancelled,
                        outcome = engine.run_turn(&ticket, task_message) => outcome,
                    };
                    terminal.finish(outcome);
                },
                move || cancel_events.finish(TurnOutcome::Cancelled),
            )
            .map_err(map_supervisor_error)?;

        let accepted = ChatAccepted {
            request_id,
            status: ChatRequestStatus::Accepted,
        };
        events.accept();
        Ok(accepted)
    }

    async fn run_turn(
        &self,
        ticket: &crate::privacy::WorkTicket,
        user_message: String,
    ) -> TurnOutcome {
        let _turn = self.turn_lock.lock().await;
        let user_tokens = match self.model.tokenize(&user_message).await {
            Ok(tokens) => tokens,
            Err(error) => return TurnOutcome::Failed(error),
        };
        let history = {
            let mut history = self.lock_history();
            history.prepare_for(user_tokens);
            history.turns().cloned().collect::<Vec<_>>()
        };
        let runtime_state = match serde_json::to_value(self.runtime.snapshot()) {
            Ok(value) => value,
            Err(_) => return TurnOutcome::Failed(ChatFailureKind::Internal),
        };
        let reply = match self
            .model
            .reply(ChatModelInput {
                persona: self.persona.clone(),
                runtime_state,
                history,
                user_message: user_message.clone(),
            })
            .await
        {
            Ok(reply) if !reply.trim().is_empty() => reply,
            Ok(_) => return TurnOutcome::Failed(ChatFailureKind::InvalidResponse),
            Err(error) => return TurnOutcome::Failed(error),
        };
        let assistant_tokens = match self.model.tokenize(&reply).await {
            Ok(tokens) => tokens,
            Err(error) => return TurnOutcome::Failed(error),
        };
        let completed = ChatTurn {
            user: user_message,
            assistant: reply.clone(),
            user_tokens,
            assistant_tokens,
        };
        let (sender, mut receiver) = mpsc::channel(1);
        match self.runtime.try_commit(ticket, &sender, completed) {
            crate::privacy::CommitOutcome::Committed => {
                let committed = receiver.try_recv().expect("committed turn is queued");
                self.lock_history().push_completed(committed);
                TurnOutcome::Replied(reply)
            }
            crate::privacy::CommitOutcome::Stale(_) => TurnOutcome::Cancelled,
            crate::privacy::CommitOutcome::QueueFull(_)
            | crate::privacy::CommitOutcome::QueueClosed(_) => {
                TurnOutcome::Failed(ChatFailureKind::Internal)
            }
        }
    }

    pub fn history(&self) -> Vec<ChatTurn> {
        self.lock_history().turns().cloned().collect()
    }

    pub fn clear(&self) {
        self.lock_history().clear();
    }

    fn next_request_id(&self) -> Result<ChatRequestId, AppError> {
        self.next_request_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map(|previous| ChatRequestId(previous + 1))
            .map_err(|_| AppError::invalid_argument("Chat request identifiers are exhausted."))
    }

    fn lock_history(&self) -> MutexGuard<'_, ChatHistory> {
        self.history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn map_supervisor_error(error: SupervisorError) -> AppError {
    match error {
        SupervisorError::QueueFull => AppError::queue_full(),
        SupervisorError::Closed => AppError::new(
            crate::error::ErrorCode::QueueClosed,
            "The runtime queue is closed.",
        ),
        _ => AppError::new(
            crate::error::ErrorCode::Internal,
            "The direct task could not be scheduled.",
        ),
    }
}

enum TurnOutcome {
    Replied(String),
    Failed(ChatFailureKind),
    Cancelled,
}

struct RequestEventState {
    accepted_sent: bool,
    terminal: Option<ChatStatusEvent>,
    finished: bool,
}

struct RequestEvents {
    request_id: ChatRequestId,
    runtime_generation: u64,
    user_message: String,
    sink: Arc<dyn ChatEventSink>,
    state: Mutex<RequestEventState>,
}

impl RequestEvents {
    fn new(
        request_id: ChatRequestId,
        runtime_generation: u64,
        user_message: String,
        sink: Arc<dyn ChatEventSink>,
    ) -> Self {
        Self {
            request_id,
            runtime_generation,
            user_message,
            sink,
            state: Mutex::new(RequestEventState {
                accepted_sent: false,
                terminal: None,
                finished: false,
            }),
        }
    }

    fn accept(&self) {
        self.sink.emit(ChatStatusEvent {
            request_id: self.request_id,
            runtime_generation: self.runtime_generation,
            status: ChatRequestStatus::Accepted,
            user_message: self.user_message.clone(),
            reply: None,
            failure: None,
        });
        let terminal = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.accepted_sent = true;
            state.terminal.take()
        };
        if let Some(terminal) = terminal {
            self.sink.emit(terminal);
        }
    }

    fn finish(&self, outcome: TurnOutcome) {
        let (status, reply, failure) = match outcome {
            TurnOutcome::Replied(reply) => (ChatRequestStatus::Replied, Some(reply), None),
            TurnOutcome::Failed(error) => (ChatRequestStatus::Failed, None, Some(error)),
            TurnOutcome::Cancelled => (ChatRequestStatus::Cancelled, None, None),
        };
        let event = ChatStatusEvent {
            request_id: self.request_id,
            runtime_generation: self.runtime_generation,
            status,
            user_message: self.user_message.clone(),
            reply,
            failure,
        };
        let emit_now = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.finished {
                return;
            }
            state.finished = true;
            if state.accepted_sent {
                true
            } else {
                state.terminal = Some(event.clone());
                false
            }
        };
        if emit_now {
            self.sink.emit(event);
        }
    }
}

struct TerminalGuard {
    events: Arc<RequestEvents>,
}

impl TerminalGuard {
    fn new(events: Arc<RequestEvents>) -> Self {
        Self { events }
    }

    fn finish(&self, outcome: TurnOutcome) {
        self.events.finish(outcome);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.events.finish(TurnOutcome::Cancelled);
    }
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
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use tempfile::{tempdir, TempDir};
    use tokio::{sync::Notify, time::timeout};

    use crate::{
        error::ErrorCode, state::RuntimeConfig, supervisor::SupervisorLimits, types::Feature,
    };

    use super::*;

    struct FakeModel {
        tokenize_results: Mutex<VecDeque<Result<usize, ChatFailureKind>>>,
        reply_results: Mutex<VecDeque<Result<String, ChatFailureKind>>>,
        tokenize_inputs: Mutex<Vec<String>>,
        reply_inputs: Mutex<Vec<ChatModelInput>>,
        reply_started: Notify,
        reply_release: Mutex<Option<Arc<Notify>>>,
    }

    impl FakeModel {
        fn new(
            tokenize_results: impl IntoIterator<Item = Result<usize, ChatFailureKind>>,
            reply_results: impl IntoIterator<Item = Result<String, ChatFailureKind>>,
        ) -> Arc<Self> {
            Arc::new(Self {
                tokenize_results: Mutex::new(tokenize_results.into_iter().collect()),
                reply_results: Mutex::new(reply_results.into_iter().collect()),
                tokenize_inputs: Mutex::new(Vec::new()),
                reply_inputs: Mutex::new(Vec::new()),
                reply_started: Notify::new(),
                reply_release: Mutex::new(None),
            })
        }

        fn successful() -> Arc<Self> {
            Self::new([Ok(3), Ok(5)], [Ok("model reply".into())])
        }

        fn block_reply(&self) -> Arc<Notify> {
            let release = Arc::new(Notify::new());
            *self
                .reply_release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&release));
            release
        }
    }

    impl ChatModel for FakeModel {
        fn tokenize<'a>(&'a self, content: &'a str) -> ChatModelFuture<'a, usize> {
            self.tokenize_inputs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(content.to_owned());
            let result = self
                .tokenize_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .unwrap_or(Err(ChatFailureKind::Internal));
            Box::pin(async move { result })
        }

        fn reply<'a>(&'a self, input: ChatModelInput) -> ChatModelFuture<'a, String> {
            self.reply_inputs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(input);
            let result = self
                .reply_results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .pop_front()
                .unwrap_or(Err(ChatFailureKind::Internal));
            let release = self
                .reply_release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            self.reply_started.notify_one();
            Box::pin(async move {
                if let Some(release) = release {
                    release.notified().await;
                }
                result
            })
        }
    }

    #[derive(Default)]
    struct FakeSink {
        events: Mutex<Vec<ChatStatusEvent>>,
        changed: Notify,
        terminal_count: AtomicUsize,
    }

    impl FakeSink {
        fn events(&self) -> Vec<ChatStatusEvent> {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        async fn wait_for_events(&self, count: usize) {
            timeout(Duration::from_secs(1), async {
                loop {
                    let changed = self.changed.notified();
                    if self.events().len() >= count {
                        return;
                    }
                    changed.await;
                }
            })
            .await
            .expect("chat events before timeout");
        }
    }

    impl ChatEventSink for FakeSink {
        fn emit(&self, event: ChatStatusEvent) {
            assert!(event.validate());
            if event.status.is_terminal() {
                self.terminal_count.fetch_add(1, Ordering::AcqRel);
            }
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(event);
            self.changed.notify_waiters();
        }
    }

    fn runtime_config(direct_capacity: usize) -> RuntimeConfig {
        RuntimeConfig {
            supervisor_limits: SupervisorLimits {
                frame_ring_capacity: 2,
                observation_queue_capacity: 1,
                direct_qwen_queue_capacity: direct_capacity,
                system_audio_segment_capacity: 2,
                user_voice_queue_capacity: 1,
                system_audio_queue_capacity: 2,
            },
            shutdown_timeout: Duration::from_millis(100),
            chat: ChatRuntimeConfig {
                history_limits: ChatHistoryLimits {
                    available_tokens: 100,
                    evict_at_per_mille: 800,
                    evict_to_per_mille: 700,
                },
                max_message_chars: 100,
                tokenize_timeout: Duration::from_secs(1),
                model_timeout: Duration::from_secs(1),
            },
            screen: crate::screen::ScreenRuntimeConfig {
                capture_interval_seconds: 3,
                frame_ring_capacity: 8,
                request_images_min: 5,
                request_images_max: 8,
                observation_min_interval_seconds: 5,
                next_check_min_seconds: 5,
                next_check_max_seconds: 18,
                next_check_default_seconds: 10,
                game_interval_seconds: 3,
                media_interval_seconds: 6,
                default_interval_seconds: 10,
                pending_observation_capacity: 1,
            },
            audio: crate::audio::AudioRuntimeConfig {
                sample_rate_hz: 16_000,
                channels: 1,
                microphone_max_seconds: 60,
                microphone_max_bytes: 2 * 1024 * 1024,
                system_audio_ring_seconds: 60,
                system_audio_segment_capacity: 2,
                user_voice_queue_capacity: 1,
                system_audio_queue_capacity: 2,
            },
        }
    }

    async fn active_runtime(direct_capacity: usize) -> (TempDir, Arc<RuntimeState>) {
        let directory = tempdir().expect("temp config directory");
        let runtime = Arc::new(
            RuntimeState::load(directory.path(), runtime_config(direct_capacity)).expect("runtime"),
        );
        runtime
            .set_privacy_paused(false)
            .await
            .expect("activate runtime");
        (directory, runtime)
    }

    fn engine(
        runtime: Arc<RuntimeState>,
        model: Arc<FakeModel>,
        sink: Arc<FakeSink>,
    ) -> Arc<ChatEngine> {
        ChatEngine::new(
            runtime,
            model,
            sink,
            "test persona".into(),
            runtime_config(2).chat,
        )
        .expect("chat engine")
    }

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

    #[tokio::test]
    async fn accepted_is_emitted_only_after_a_direct_reservation_succeeds() {
        let (_directory, runtime) = active_runtime(1).await;
        let model = FakeModel::successful();
        let release = model.block_reply();
        let sink = Arc::new(FakeSink::default());
        let engine = engine(Arc::clone(&runtime), model, Arc::clone(&sink));

        let accepted = engine
            .submit_text(" first ".into())
            .expect("first accepted");
        assert_eq!(accepted.request_id, ChatRequestId(1));
        assert_eq!(runtime.supervisor().queue_lengths().direct_queue, 1);
        assert_eq!(sink.events()[0].user_message, "first");

        let error = engine
            .submit_text("second".into())
            .expect_err("full direct lane");
        assert_eq!(error.code, ErrorCode::QueueFull);
        assert_eq!(sink.events().len(), 1);

        runtime.set_privacy_paused(true).await.expect("pause");
        release.notify_waiters();
        sink.wait_for_events(2).await;
        assert_eq!(sink.terminal_count.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn successful_reply_emits_one_terminal_and_commits_model_token_counts() {
        let (_directory, runtime) = active_runtime(2).await;
        let model = FakeModel::successful();
        let sink = Arc::new(FakeSink::default());
        let engine = engine(runtime, Arc::clone(&model), Arc::clone(&sink));

        engine.submit_text("hello".into()).expect("accepted");
        sink.wait_for_events(2).await;

        let events = sink.events();
        assert_eq!(
            events.iter().map(|event| event.status).collect::<Vec<_>>(),
            [ChatRequestStatus::Accepted, ChatRequestStatus::Replied]
        );
        assert_eq!(events[1].reply.as_deref(), Some("model reply"));
        assert_eq!(sink.terminal_count.load(Ordering::Acquire), 1);
        assert_eq!(
            model
                .tokenize_inputs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            ["hello", "model reply"]
        );
        assert_eq!(
            engine.history(),
            [ChatTurn {
                user: "hello".into(),
                assistant: "model reply".into(),
                user_tokens: 3,
                assistant_tokens: 5,
            }]
        );

        engine.clear();
        assert!(engine.history().is_empty());
    }

    #[tokio::test]
    async fn model_failures_map_to_one_failed_terminal_event() {
        for (reply, expected) in [
            (
                Err(ChatFailureKind::ModelUnavailable),
                ChatFailureKind::ModelUnavailable,
            ),
            (Err(ChatFailureKind::Timeout), ChatFailureKind::Timeout),
            (Ok("   ".into()), ChatFailureKind::InvalidResponse),
        ] {
            let (_directory, runtime) = active_runtime(1).await;
            let model = FakeModel::new([Ok(1)], [reply]);
            let sink = Arc::new(FakeSink::default());
            let engine = engine(runtime, model, Arc::clone(&sink));

            engine.submit_text("hello".into()).expect("accepted");
            sink.wait_for_events(2).await;

            let events = sink.events();
            assert_eq!(events[0].status, ChatRequestStatus::Accepted);
            assert_eq!(events[1].status, ChatRequestStatus::Failed);
            assert_eq!(events[1].failure, Some(expected));
            assert_eq!(sink.terminal_count.load(Ordering::Acquire), 1);
            assert!(engine.history().is_empty());
        }
    }

    #[tokio::test]
    async fn unavailable_engine_fails_explicitly_without_a_fallback_model() {
        let (_directory, runtime) = active_runtime(1).await;
        let sink = Arc::new(FakeSink::default());
        let engine = ChatEngine::unavailable(
            runtime,
            Arc::clone(&sink) as Arc<dyn ChatEventSink>,
            runtime_config(1).chat,
        )
        .expect("unavailable chat engine");

        engine.submit_text("hello".into()).expect("accepted");
        sink.wait_for_events(2).await;

        let events = sink.events();
        assert_eq!(events[0].status, ChatRequestStatus::Accepted);
        assert_eq!(events[1].status, ChatRequestStatus::Failed);
        assert_eq!(events[1].failure, Some(ChatFailureKind::ModelUnavailable));
        assert!(engine.history().is_empty());
    }

    #[tokio::test]
    async fn pause_cancels_an_in_flight_reply_once_without_committing_history() {
        let (_directory, runtime) = active_runtime(1).await;
        let model = FakeModel::successful();
        let release = model.block_reply();
        let sink = Arc::new(FakeSink::default());
        let engine = engine(Arc::clone(&runtime), Arc::clone(&model), Arc::clone(&sink));

        engine.submit_text("hello".into()).expect("accepted");
        timeout(Duration::from_secs(1), model.reply_started.notified())
            .await
            .expect("model reply started");
        runtime.set_privacy_paused(true).await.expect("pause");
        release.notify_waiters();
        sink.wait_for_events(2).await;

        let events = sink.events();
        assert_eq!(
            events.iter().map(|event| event.status).collect::<Vec<_>>(),
            [ChatRequestStatus::Accepted, ChatRequestStatus::Cancelled]
        );
        assert_eq!(sink.terminal_count.load(Ordering::Acquire), 1);
        assert!(engine.history().is_empty());
    }

    #[tokio::test]
    async fn disabling_proactive_responses_does_not_cancel_direct_chat() {
        let (_directory, runtime) = active_runtime(1).await;
        let model = FakeModel::successful();
        let release = model.block_reply();
        let sink = Arc::new(FakeSink::default());
        let engine = engine(Arc::clone(&runtime), Arc::clone(&model), Arc::clone(&sink));

        engine.submit_text("hello".into()).expect("accepted");
        timeout(Duration::from_secs(1), model.reply_started.notified())
            .await
            .expect("model reply started");
        runtime
            .set_feature_enabled(Feature::ProactiveResponse, false)
            .await
            .expect("disable proactive responses");
        release.notify_waiters();
        sink.wait_for_events(2).await;

        assert_eq!(sink.events()[1].status, ChatRequestStatus::Replied);
        assert_eq!(sink.terminal_count.load(Ordering::Acquire), 1);
        assert_eq!(engine.history().len(), 1);
    }
}
