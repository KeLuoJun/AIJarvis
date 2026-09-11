use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tempfile::Builder;

use crate::error::{AppError, ErrorCode};

const PERSONAS_FILE: &str = "personas.json";
const SCHEMA_VERSION: u32 = 1;
const MAX_PERSONAS: usize = 12;
const MAX_NAME_CHARS: usize = 32;
const MAX_PROMPT_CHARS: usize = 8_000;
const MAX_FILE_BYTES: usize = 256 * 1024;
const PERSIST_RETRY_LIMIT: usize = 64;
const PERSIST_RETRY_DELAY: Duration = Duration::from_millis(2);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PersonaProfile {
    pub id: String,
    pub name: String,
    pub prompt: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PersonaState {
    pub active_id: String,
    pub personas: Vec<PersonaProfile>,
}

impl PersonaState {
    pub fn active(&self) -> Option<&PersonaProfile> {
        self.personas
            .iter()
            .find(|persona| persona.id == self.active_id)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersonaFile {
    schema_version: u32,
    active_id: String,
    personas: Vec<PersonaProfile>,
}

pub struct PersonaManager {
    path: PathBuf,
    state: Mutex<PersonaState>,
}

#[derive(Clone)]
pub enum PersonaSource {
    Static(String),
    Managed(Arc<PersonaManager>),
}

impl PersonaSource {
    pub fn current_prompt(&self) -> String {
        match self {
            Self::Static(prompt) => prompt.clone(),
            Self::Managed(manager) => manager.active_prompt(),
        }
    }
}

impl PersonaManager {
    pub fn load(config_dir: &Path, default_prompt: &str) -> Result<Arc<Self>, AppError> {
        let default_state = || PersonaState {
            active_id: "default".into(),
            personas: vec![PersonaProfile {
                id: "default".into(),
                name: "默认伙伴".into(),
                prompt: default_prompt.trim().to_owned(),
            }],
        };
        let path = config_dir.join(PERSONAS_FILE);
        let state = match read_file(&path)? {
            None => default_state(),
            Some(bytes) => {
                let file: PersonaFile = serde_json::from_slice(&bytes).map_err(|_| read_error())?;
                if file.schema_version != SCHEMA_VERSION {
                    return Err(read_error());
                }
                let candidate = PersonaState {
                    active_id: file.active_id,
                    personas: file.personas,
                };
                validate_state(&candidate)?;
                candidate
            }
        };
        let manager = Arc::new(Self {
            path,
            state: Mutex::new(state),
        });
        if !manager.path.exists() {
            manager.persist(&manager.snapshot())?;
        }
        Ok(manager)
    }

    pub fn snapshot(&self) -> PersonaState {
        self.lock_state().clone()
    }

    pub fn active_prompt(&self) -> String {
        self.lock_state()
            .active()
            .map(|persona| persona.prompt.clone())
            .unwrap_or_default()
    }

    pub fn create(&self, name: String, prompt: String) -> Result<PersonaState, AppError> {
        let (name, prompt) = validate_fields(name, prompt)?;
        let mut state = self.lock_state();
        if state.personas.len() >= MAX_PERSONAS {
            return Err(AppError::invalid_argument(
                "The persona limit has been reached.",
            ));
        }
        let original = state.clone();
        let id = next_id(&state);
        state.personas.push(PersonaProfile {
            id: id.clone(),
            name,
            prompt,
        });
        state.active_id = id;
        if let Err(error) = self.persist(&state) {
            *state = original;
            return Err(error);
        }
        Ok(state.clone())
    }

    pub fn update(&self, id: &str, name: String, prompt: String) -> Result<PersonaState, AppError> {
        let (name, prompt) = validate_fields(name, prompt)?;
        let mut state = self.lock_state();
        let original = state.clone();
        let persona = state
            .personas
            .iter_mut()
            .find(|persona| persona.id == id)
            .ok_or_else(|| AppError::invalid_argument("The persona does not exist."))?;
        persona.name = name;
        persona.prompt = prompt;
        if let Err(error) = self.persist(&state) {
            *state = original;
            return Err(error);
        }
        Ok(state.clone())
    }

    pub fn select(&self, id: &str) -> Result<PersonaState, AppError> {
        let mut state = self.lock_state();
        if !state.personas.iter().any(|persona| persona.id == id) {
            return Err(AppError::invalid_argument("The persona does not exist."));
        }
        let original = state.clone();
        state.active_id = id.to_owned();
        if let Err(error) = self.persist(&state) {
            *state = original;
            return Err(error);
        }
        Ok(state.clone())
    }

    pub fn delete(&self, id: &str) -> Result<PersonaState, AppError> {
        let mut state = self.lock_state();
        if state.personas.len() == 1 {
            return Err(AppError::invalid_argument(
                "The last persona cannot be deleted.",
            ));
        }
        let original = state.clone();
        let index = state
            .personas
            .iter()
            .position(|persona| persona.id == id)
            .ok_or_else(|| AppError::invalid_argument("The persona does not exist."))?;
        state.personas.remove(index);
        if state.active_id == id {
            state.active_id = state.personas[0].id.clone();
        }
        if let Err(error) = self.persist(&state) {
            *state = original;
            return Err(error);
        }
        Ok(state.clone())
    }

    fn persist(&self, state: &PersonaState) -> Result<(), AppError> {
        let file = PersonaFile {
            schema_version: SCHEMA_VERSION,
            active_id: state.active_id.clone(),
            personas: state.personas.clone(),
        };
        let serialized = serde_json::to_vec_pretty(&file).map_err(|_| write_error())?;
        if serialized.len().saturating_add(1) > MAX_FILE_BYTES {
            return Err(write_error());
        }
        let directory = self.path.parent().ok_or_else(write_error)?;
        fs::create_dir_all(directory).map_err(|_| write_error())?;
        let mut temporary = Builder::new()
            .prefix(".ai-jarvis-personas-")
            .suffix(".tmp")
            .tempfile_in(directory)
            .map_err(|_| write_error())?;
        temporary
            .write_all(&serialized)
            .and_then(|()| temporary.write_all(b"\n"))
            .and_then(|()| temporary.flush())
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|_| write_error())?;
        let mut pending = temporary;
        for attempt in 0..=PERSIST_RETRY_LIMIT {
            match pending.persist(&self.path) {
                Ok(_) => return Ok(()),
                Err(failure) => {
                    if attempt == PERSIST_RETRY_LIMIT || !retryable(&failure.error) {
                        return Err(write_error());
                    }
                    pending = failure.file;
                    thread::sleep(PERSIST_RETRY_DELAY);
                }
            }
        }
        unreachable!("bounded persist loop always returns")
    }

    fn lock_state(&self) -> MutexGuard<'_, PersonaState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn validate_fields(name: String, prompt: String) -> Result<(String, String), AppError> {
    let name = name.trim().to_owned();
    let prompt = prompt.trim().to_owned();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_CHARS
        || prompt.is_empty()
        || prompt.chars().count() > MAX_PROMPT_CHARS
    {
        return Err(AppError::invalid_argument(
            "The persona fields are invalid.",
        ));
    }
    Ok((name, prompt))
}

fn validate_state(state: &PersonaState) -> Result<(), AppError> {
    if state.personas.is_empty()
        || state.personas.len() > MAX_PERSONAS
        || state.active().is_none()
        || state.personas.iter().any(|persona| {
            persona.id.trim().is_empty()
                || validate_fields(persona.name.clone(), persona.prompt.clone()).is_err()
        })
    {
        return Err(read_error());
    }
    let mut ids = state
        .personas
        .iter()
        .map(|persona| persona.id.as_str())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    if ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(read_error());
    }
    Ok(())
}

fn next_id(state: &PersonaState) -> String {
    (1_u64..)
        .map(|sequence| format!("persona-{sequence}"))
        .find(|id| !state.personas.iter().any(|persona| &persona.id == id))
        .expect("finite persona collection always has a free identifier")
}

fn read_file(path: &Path) -> Result<Option<Vec<u8>>, AppError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(read_error()),
    };
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| read_error())?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(read_error());
    }
    Ok(Some(bytes))
}

fn read_error() -> AppError {
    AppError::new(
        ErrorCode::ConfigRead,
        "Could not read persona configuration.",
    )
}

fn write_error() -> AppError {
    AppError::new(
        ErrorCode::ConfigWrite,
        "Could not write persona configuration.",
    )
}

#[cfg(windows)]
fn retryable(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32))
}

#[cfg(not(windows))]
fn retryable(_error: &std::io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    const DEFAULT_PROMPT: &str = "说话直接，但尊重事实。";

    #[test]
    fn missing_store_creates_one_selected_default_persona() {
        let directory = tempdir().expect("temporary directory");
        let manager = PersonaManager::load(directory.path(), DEFAULT_PROMPT).expect("personas");

        let state = manager.snapshot();
        assert_eq!(state.personas.len(), 1);
        assert_eq!(state.active_id, state.personas[0].id);
        assert_eq!(state.personas[0].name, "默认伙伴");
        assert_eq!(manager.active_prompt(), DEFAULT_PROMPT);
    }

    #[test]
    fn create_update_select_delete_round_trips_through_disk() {
        let directory = tempdir().expect("temporary directory");
        let manager = PersonaManager::load(directory.path(), DEFAULT_PROMPT).expect("personas");

        let created = manager
            .create("代码搭档".into(), "简洁、严谨。".into())
            .expect("create");
        manager.select(&created.active_id).expect("select");
        manager
            .update(
                &created.active_id,
                "产品搭档".into(),
                "先问目标，再给判断。".into(),
            )
            .expect("update");

        let reloaded = PersonaManager::load(directory.path(), DEFAULT_PROMPT).expect("reload");
        assert_eq!(reloaded.active_prompt(), "先问目标，再给判断。");
        assert_eq!(
            reloaded.snapshot().active().expect("active").name,
            "产品搭档"
        );

        reloaded.delete(&created.active_id).expect("delete");
        assert_eq!(reloaded.snapshot().personas.len(), 1);
        assert!(fs::metadata(directory.path().join("personas.json")).is_ok());
    }

    #[test]
    fn rejects_empty_oversized_and_last_persona_mutations() {
        let directory = tempdir().expect("temporary directory");
        let manager = PersonaManager::load(directory.path(), DEFAULT_PROMPT).expect("personas");
        let default_id = manager.snapshot().active_id;

        assert!(manager.create("".into(), "prompt".into()).is_err());
        assert!(manager.create("name".into(), "".into()).is_err());
        assert!(manager.create("x".repeat(33), "prompt".into()).is_err());
        assert!(manager.create("name".into(), "x".repeat(8_001)).is_err());
        assert!(manager.delete(&default_id).is_err());
        assert!(manager.select("missing").is_err());
    }
}
