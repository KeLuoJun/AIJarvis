use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use serde_json::Value;

const MAX_IMAGES: usize = 3;
const MAX_IMAGE_AGE: Duration = Duration::from_secs(30);
const MAX_AUDIO_ITEMS: usize = 128;
const MAX_AUDIO_CHARS: usize = 32_000;
const MAX_AUDIO_AGE: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PerceptionSnapshot {
    pub image_data_urls: Vec<String>,
    pub image_captured_at: Vec<Instant>,
    pub audio_observations: String,
    pub foreground_application: String,
    pub foreground_window_title: String,
}

#[derive(Default)]
struct PerceptionState {
    images: VecDeque<(String, Instant)>,
    audio: VecDeque<(Value, Instant)>,
    foreground_application: String,
    foreground_window_title: String,
    foreground_observed_at: Option<Instant>,
}

#[derive(Default)]
pub struct PerceptionContext {
    state: Mutex<PerceptionState>,
}

impl PerceptionContext {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn update(
        &self,
        images: Vec<String>,
        captured_at: Vec<Instant>,
        audio_observations: &str,
        foreground_application: String,
        foreground_window_title: String,
    ) {
        self.update_at(
            images,
            captured_at,
            audio_observations,
            foreground_application,
            foreground_window_title,
            Instant::now(),
        );
    }

    fn update_at(
        &self,
        images: Vec<String>,
        captured_at: Vec<Instant>,
        audio_observations: &str,
        foreground_application: String,
        foreground_window_title: String,
        now: Instant,
    ) {
        let mut state = self.lock_state();
        prune(&mut state, now);
        for (image, at) in images.into_iter().zip(captured_at) {
            if !image.trim().is_empty() && now.saturating_duration_since(at) <= MAX_IMAGE_AGE {
                state.images.push_back((image, at));
            }
        }
        while state.images.len() > MAX_IMAGES {
            state.images.pop_front();
        }

        if let Ok(Value::Array(values)) = serde_json::from_str(audio_observations) {
            for value in values.into_iter().take(16) {
                let Some(text) = value.get("text").and_then(Value::as_str) else {
                    continue;
                };
                if text.trim().is_empty() || text.chars().count() > 16_000 {
                    continue;
                }
                state.audio.push_back((value, now));
            }
        }
        while state.audio.len() > MAX_AUDIO_ITEMS || audio_chars(&state.audio) > MAX_AUDIO_CHARS {
            state.audio.pop_front();
        }

        if !foreground_application.is_empty() || !foreground_window_title.is_empty() {
            state.foreground_application = foreground_application;
            state.foreground_window_title = foreground_window_title;
            state.foreground_observed_at = Some(now);
        }
    }

    pub fn snapshot(&self) -> PerceptionSnapshot {
        self.snapshot_at(Instant::now())
    }

    pub fn conversation_snapshot(&self) -> PerceptionSnapshot {
        self.conversation_snapshot_at(Instant::now())
    }

    fn snapshot_at(&self, now: Instant) -> PerceptionSnapshot {
        let mut state = self.lock_state();
        prune(&mut state, now);
        build_snapshot(&state)
    }

    fn conversation_snapshot_at(&self, now: Instant) -> PerceptionSnapshot {
        let mut state = self.lock_state();
        prune_audio(&mut state, now);
        build_snapshot(&state)
    }

    pub fn clear(&self) {
        *self.lock_state() = PerceptionState::default();
    }

    pub fn clear_images(&self) {
        let mut state = self.lock_state();
        state.images.clear();
        state.foreground_application.clear();
        state.foreground_window_title.clear();
        state.foreground_observed_at = None;
    }

    pub fn clear_audio(&self) {
        self.lock_state().audio.clear();
    }

    fn lock_state(&self) -> MutexGuard<'_, PerceptionState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn build_snapshot(state: &PerceptionState) -> PerceptionSnapshot {
    let audio = state
        .audio
        .iter()
        .map(|(value, _)| value.clone())
        .collect::<Vec<_>>();
    PerceptionSnapshot {
        image_data_urls: state
            .images
            .iter()
            .map(|(image, _)| image.clone())
            .collect(),
        image_captured_at: state.images.iter().map(|(_, at)| *at).collect(),
        audio_observations: if audio.is_empty() {
            String::new()
        } else {
            serde_json::to_string(&audio).expect("perception audio uses JSON values")
        },
        foreground_application: state.foreground_application.clone(),
        foreground_window_title: state.foreground_window_title.clone(),
    }
}

fn prune(state: &mut PerceptionState, now: Instant) {
    state
        .images
        .retain(|(_, at)| now.saturating_duration_since(*at) <= MAX_IMAGE_AGE);
    prune_audio(state, now);
    if state
        .foreground_observed_at
        .is_some_and(|at| now.saturating_duration_since(at) > MAX_IMAGE_AGE)
    {
        state.foreground_application.clear();
        state.foreground_window_title.clear();
        state.foreground_observed_at = None;
    }
}

fn prune_audio(state: &mut PerceptionState, now: Instant) {
    state
        .audio
        .retain(|(_, at)| now.saturating_duration_since(*at) <= MAX_AUDIO_AGE);
}

fn audio_chars(audio: &VecDeque<(Value, Instant)>) -> usize {
    audio
        .iter()
        .filter_map(|(value, _)| value.get("text").and_then(Value::as_str))
        .map(str::len)
        .sum()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn snapshot_keeps_recent_images_audio_and_foreground_context_bounded() {
        let context = PerceptionContext::new();
        let now = Instant::now();
        context.update(
            vec![
                "image-1".into(),
                "image-2".into(),
                "image-3".into(),
                "image-4".into(),
            ],
            vec![now; 4],
            r#"[{"segmentSequence":1,"capturedAtMs":1,"text":"正在播放课程"}]"#,
            "player.exe".into(),
            "第一章".into(),
        );

        let snapshot = context.snapshot();
        assert_eq!(snapshot.image_data_urls, ["image-2", "image-3", "image-4"]);
        assert!(snapshot.audio_observations.contains("正在播放课程"));
        assert_eq!(snapshot.foreground_application, "player.exe");
        assert_eq!(snapshot.foreground_window_title, "第一章");
    }

    #[test]
    fn stale_images_and_audio_are_removed_as_a_sliding_window() {
        let context = PerceptionContext::new();
        let old = Instant::now() - Duration::from_secs(601);
        context.update_at(
            vec!["old-image".into()],
            vec![old],
            r#"[{"segmentSequence":1,"capturedAtMs":1,"text":"old audio"}]"#,
            String::new(),
            String::new(),
            old,
        );

        let snapshot = context.snapshot_at(Instant::now());
        assert!(snapshot.image_data_urls.is_empty());
        assert!(snapshot.audio_observations.is_empty());
    }

    #[test]
    fn direct_conversation_retains_the_last_external_screen_for_follow_up() {
        let context = PerceptionContext::new();
        let captured = Instant::now() - Duration::from_secs(45);
        context.update_at(
            vec!["last-external-screen".into()],
            vec![captured],
            "[]",
            "editor.exe".into(),
            "Document".into(),
            captured,
        );

        let snapshot = context.conversation_snapshot_at(Instant::now());
        assert_eq!(snapshot.image_data_urls, ["last-external-screen"]);
        assert_eq!(snapshot.foreground_application, "editor.exe");
    }

    #[test]
    fn clear_drops_all_sensitive_context() {
        let context = PerceptionContext::new();
        context.update(
            vec!["image".into()],
            vec![Instant::now()],
            r#"[{"text":"audio"}]"#,
            "app".into(),
            "title".into(),
        );
        context.clear();

        assert_eq!(context.snapshot(), PerceptionSnapshot::default());
    }
}
