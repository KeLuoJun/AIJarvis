use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use tempfile::Builder;

use crate::error::AppError;

const MEMORY_FILE: &str = "memories.json";
const SCHEMA_VERSION: u32 = 1;
const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_MEMORIES: usize = 128;
const MAX_TEXT_CHARS: usize = 512;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum MemoryKind {
    Preference,
    Fact,
    Task,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryItem {
    pub id: u64,
    pub kind: MemoryKind,
    pub text: String,
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryCandidate {
    pub kind: MemoryKind,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MemoryFile {
    schema_version: u32,
    enabled: bool,
    next_id: u64,
    items: Vec<MemoryItem>,
}

struct MemoryState {
    enabled: bool,
    next_id: u64,
    items: Vec<MemoryItem>,
}

pub struct MemoryStore {
    path: PathBuf,
    state: Mutex<MemoryState>,
}

impl MemoryStore {
    pub fn load(config_dir: &Path) -> Result<Arc<Self>, AppError> {
        let path = config_dir.join(MEMORY_FILE);
        let state = match File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take((MAX_FILE_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(|_| read_error())?;
                if bytes.len() > MAX_FILE_BYTES {
                    return Err(read_error());
                }
                let file: MemoryFile = serde_json::from_slice(&bytes).map_err(|_| read_error())?;
                if file.schema_version != SCHEMA_VERSION
                    || file.items.len() > MAX_MEMORIES
                    || file.items.iter().any(|item| item.text.trim().is_empty())
                {
                    return Err(read_error());
                }
                MemoryState {
                    enabled: file.enabled,
                    next_id: file.next_id.max(1),
                    items: file.items,
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => MemoryState {
                enabled: true,
                next_id: 1,
                items: Vec::new(),
            },
            Err(_) => return Err(read_error()),
        };
        Ok(Arc::new(Self {
            path,
            state: Mutex::new(state),
        }))
    }

    pub fn snapshot(&self) -> (bool, Vec<MemoryItem>) {
        let state = self.lock();
        (state.enabled, state.items.clone())
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<(), AppError> {
        let mut state = self.lock();
        state.enabled = enabled;
        self.persist(&state)
    }

    pub fn add(&self, kind: MemoryKind, text: String) -> Result<MemoryItem, AppError> {
        let text = text.trim().to_owned();
        if text.is_empty() || text.chars().count() > MAX_TEXT_CHARS {
            return Err(AppError::invalid_argument(
                "The memory text is empty or too long.",
            ));
        }
        let mut state = self.lock();
        if state.items.len() >= MAX_MEMORIES {
            return Err(AppError::queue_full());
        }
        let now = now_seconds();
        let item = MemoryItem {
            id: state.next_id,
            kind,
            text,
            created_at: now,
            updated_at: now,
        };
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or_else(|| AppError::invalid_argument("Memory identifiers are exhausted."))?;
        state.items.push(item.clone());
        self.persist(&state)?;
        Ok(item)
    }

    pub fn delete(&self, id: u64) -> Result<(), AppError> {
        let mut state = self.lock();
        state.items.retain(|item| item.id != id);
        self.persist(&state)
    }

    pub fn update(&self, id: u64, kind: MemoryKind, text: String) -> Result<MemoryItem, AppError> {
        let text = text.trim().to_owned();
        if text.is_empty() || text.chars().count() > MAX_TEXT_CHARS {
            return Err(AppError::invalid_argument(
                "The memory text is empty or too long.",
            ));
        }
        let mut state = self.lock();
        let item = state
            .items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or_else(|| AppError::invalid_argument("The memory does not exist."))?;
        item.kind = kind;
        item.text = text;
        item.updated_at = now_seconds();
        let updated = item.clone();
        self.persist(&state)?;
        Ok(updated)
    }

    pub fn add_candidates(&self, candidates: Vec<MemoryCandidate>) {
        for candidate in candidates {
            let text = candidate.text.trim().to_owned();
            let duplicate = self
                .lock()
                .items
                .iter()
                .any(|item| item.text.eq_ignore_ascii_case(&text));
            if !duplicate {
                let _ = self.add(candidate.kind, text);
            }
        }
    }

    pub fn clear(&self) -> Result<(), AppError> {
        let mut state = self.lock();
        state.items.clear();
        self.persist(&state)
    }

    pub fn prompt_context(&self, query: &str) -> String {
        let state = self.lock();
        if !state.enabled || state.items.is_empty() {
            return "[]".to_owned();
        }
        let terms = query
            .split(|ch: char| !ch.is_alphanumeric() && !('\u{4e00}'..='\u{9fff}').contains(&ch))
            .filter(|term| term.chars().count() >= 2)
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        let mut matches = state
            .items
            .iter()
            .filter_map(|item| {
                let lower = item.text.to_lowercase();
                let score = terms
                    .iter()
                    .filter(|term| lower.contains(term.as_str()))
                    .count();
                (terms.is_empty() || score > 0).then_some((score, item))
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| right.1.updated_at.cmp(&left.1.updated_at))
        });
        let items = matches
            .into_iter()
            .take(8)
            .map(|(_, item)| serde_json::json!({ "kind": item.kind, "text": item.text, "updatedAt": item.updated_at }))
            .collect::<Vec<_>>();
        serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_owned())
    }

    fn persist(&self, state: &MemoryState) -> Result<(), AppError> {
        let contents = serde_json::to_vec_pretty(&MemoryFile {
            schema_version: SCHEMA_VERSION,
            enabled: state.enabled,
            next_id: state.next_id,
            items: state.items.clone(),
        })
        .map_err(|_| write_error())?;
        if contents.len() + 1 > MAX_FILE_BYTES {
            return Err(write_error());
        }
        let directory = self.path.parent().ok_or_else(write_error)?;
        fs::create_dir_all(directory).map_err(|_| write_error())?;
        let mut file = Builder::new()
            .prefix(".ai-jarvis-memory-")
            .suffix(".tmp")
            .tempfile_in(directory)
            .map_err(|_| write_error())?;
        file.write_all(&contents)
            .and_then(|()| file.write_all(b"\n"))
            .and_then(|()| file.flush())
            .and_then(|()| file.as_file().sync_all())
            .map_err(|_| write_error())?;
        file.persist(&self.path).map_err(|_| write_error())?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}
fn read_error() -> AppError {
    AppError::new(
        crate::error::ErrorCode::ConfigRead,
        "Could not read memories.",
    )
}
fn write_error() -> AppError {
    AppError::new(
        crate::error::ErrorCode::ConfigWrite,
        "Could not write memories.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn memories_round_trip_and_recall_by_term() {
        let dir = tempdir().expect("tempdir");
        let store = MemoryStore::load(dir.path()).expect("load");
        store
            .add(MemoryKind::Preference, "喜欢简洁的 Rust 代码".into())
            .expect("add");
        assert!(store.prompt_context("Rust").contains("简洁"));
        let (enabled, items) = store.snapshot();
        assert!(enabled);
        assert_eq!(items.len(), 1);
        let restored = MemoryStore::load(dir.path()).expect("reload");
        assert_eq!(restored.snapshot().1, items);
    }

    #[test]
    fn candidate_deduplication_preserves_existing_items_and_input_order() {
        let dir = tempdir().expect("tempdir");
        let store = MemoryStore::load(dir.path()).expect("load");
        let existing = store.add(MemoryKind::Fact, "Rust".into()).expect("add");
        store.add_candidates(vec![
            MemoryCandidate {
                kind: MemoryKind::Task,
                text: " rust ".into(),
            },
            MemoryCandidate {
                kind: MemoryKind::Preference,
                text: " TypeScript ".into(),
            },
            MemoryCandidate {
                kind: MemoryKind::Fact,
                text: "TYPESCRIPT".into(),
            },
        ]);
        let items = store.snapshot().1;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0], existing);
        assert_eq!(items[1].text, "TypeScript");
        assert_eq!(items[1].kind, MemoryKind::Preference);
        assert_eq!(
            MemoryStore::load(dir.path()).expect("reload").snapshot().1,
            items
        );
    }

    #[test]
    fn disabled_memory_is_not_recalled() {
        let dir = tempdir().expect("tempdir");
        let store = MemoryStore::load(dir.path()).expect("load");
        store
            .add(MemoryKind::Fact, "用户在上海".into())
            .expect("add");
        store.set_enabled(false).expect("disable");
        assert_eq!(store.prompt_context("上海"), "[]");
    }
}
