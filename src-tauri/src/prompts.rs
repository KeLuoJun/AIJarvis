use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
};

use jsonschema::JSONSchema;
use minijinja::Environment;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{AppError, ErrorCode};

const MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptId {
    Observation,
    DirectChat,
}

impl PromptId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observation => "observation",
            Self::DirectChat => "direct_chat",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Manifest {
    schema_version: u32,
    prompt: Vec<PromptEntry>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
struct PromptEntry {
    id: String,
    system: String,
    user: String,
    schema: String,
    required_variables: Vec<String>,
    max_output_tokens: u32,
    sampling_profile: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedPrompt {
    pub id: PromptId,
    pub system: String,
    pub user: String,
    pub schema: Value,
    pub max_output_tokens: u32,
    pub sampling_profile: String,
}

#[derive(Clone, Debug)]
pub struct PromptStore {
    resource_dir: PathBuf,
    manifest: Manifest,
}

impl PromptStore {
    pub fn load(resource_dir: impl AsRef<Path>) -> Result<Self, AppError> {
        let resource_dir = resource_dir.as_ref().to_path_buf();
        let path = resource_dir.join("resources/prompts/manifest.toml");
        let bytes = fs::read(&path).map_err(|_| prompt_error("prompt manifest is unavailable"))?;
        let text =
            std::str::from_utf8(&bytes).map_err(|_| prompt_error("prompt manifest is invalid"))?;
        let manifest = toml::from_str::<Manifest>(text)
            .map_err(|_| prompt_error("prompt manifest is invalid"))?;
        if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(prompt_error("prompt manifest schema is unsupported"));
        }
        for entry in &manifest.prompt {
            validate_entry(entry)?;
        }
        let ids = manifest
            .prompt
            .iter()
            .map(|entry| entry.id.as_str())
            .collect::<BTreeSet<_>>();
        if ids
            != BTreeSet::from([
                PromptId::Observation.as_str(),
                PromptId::DirectChat.as_str(),
            ])
            || ids.len() != manifest.prompt.len()
        {
            return Err(prompt_error(
                "prompt manifest must declare each prompt exactly once",
            ));
        }
        Ok(Self {
            resource_dir,
            manifest,
        })
    }

    pub fn render(
        &self,
        id: PromptId,
        variables: &BTreeMap<String, Value>,
    ) -> Result<RenderedPrompt, AppError> {
        let entry = self
            .manifest
            .prompt
            .iter()
            .find(|entry| entry.id == id.as_str())
            .ok_or_else(|| prompt_error("requested prompt is not declared"))?;
        for required in &entry.required_variables {
            if !variables.contains_key(required) {
                return Err(prompt_error("required prompt variable is missing"));
            }
        }
        let prompt_root = self.resource_dir.join("resources/prompts");
        let system_path = safe_join(&prompt_root, &entry.system)
            .ok_or_else(|| prompt_error("prompt path is outside the resource directory"))?;
        let user_path = safe_join(&prompt_root, &entry.user)
            .ok_or_else(|| prompt_error("prompt path is outside the resource directory"))?;
        let schema_path = safe_schema_path(&self.resource_dir, &entry.schema)
            .ok_or_else(|| prompt_error("schema path is outside the resource directory"))?;
        let system = fs::read_to_string(system_path)
            .map_err(|_| prompt_error("system prompt is unavailable"))?;
        let user_template = fs::read_to_string(user_path)
            .map_err(|_| prompt_error("user prompt is unavailable"))?;
        let schema_bytes =
            fs::read(schema_path).map_err(|_| prompt_error("response schema is unavailable"))?;
        let schema: Value = serde_json::from_slice(&schema_bytes)
            .map_err(|_| prompt_error("response schema is invalid"))?;
        if system.trim().is_empty()
            || schema.get("type").and_then(Value::as_str) != Some("object")
            || schema.get("additionalProperties").and_then(Value::as_bool) != Some(false)
        {
            return Err(prompt_error("prompt or response schema is not strict"));
        }
        JSONSchema::compile(&schema)
            .map_err(|_| prompt_error("response schema cannot be compiled"))?;
        let mut environment = Environment::new();
        environment.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
        let template = environment
            .template_from_str(&user_template)
            .map_err(|_| prompt_error("user prompt template is invalid"))?;
        let user = template
            .render(variables)
            .map_err(|_| prompt_error("user prompt variables cannot be rendered"))?;
        if user.trim().is_empty() {
            return Err(prompt_error("rendered user prompt is empty"));
        }
        Ok(RenderedPrompt {
            id,
            system,
            user,
            schema,
            max_output_tokens: entry.max_output_tokens,
            sampling_profile: entry.sampling_profile.clone(),
        })
    }
}

fn validate_entry(entry: &PromptEntry) -> Result<(), AppError> {
    let id_ok = matches!(entry.id.as_str(), "observation" | "direct_chat");
    if !id_ok
        || entry.required_variables.is_empty()
        || entry.max_output_tokens == 0
        || entry.sampling_profile.is_empty()
    {
        return Err(prompt_error("prompt manifest entry is invalid"));
    }
    for path in [&entry.system, &entry.user] {
        if safe_relative(path).is_none() {
            return Err(prompt_error("prompt manifest path is invalid"));
        }
    }
    if safe_schema_relative(&entry.schema).is_none() {
        return Err(prompt_error("prompt schema path is invalid"));
    }
    Ok(())
}

fn safe_join(root: &Path, relative: &str) -> Option<PathBuf> {
    safe_relative(relative).map(|path| root.join(path))
}

fn safe_schema_path(resource_dir: &Path, relative: &str) -> Option<PathBuf> {
    safe_schema_relative(relative).map(|path| resource_dir.join("resources/schemas").join(path))
}

fn safe_schema_relative(relative: &str) -> Option<PathBuf> {
    let path = Path::new(relative);
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::ParentDir))
        || !matches!(components.next(), Some(Component::Normal(name)) if name == "schemas")
    {
        return None;
    }
    let remainder = components.collect::<PathBuf>();
    safe_relative(remainder.to_str()?).map(Path::to_path_buf)
}

fn safe_relative(value: &str) -> Option<&Path> {
    let path = Path::new(value);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        None
    } else {
        Some(path)
    }
}

fn prompt_error(message: &str) -> AppError {
    AppError::new(ErrorCode::ConfigRead, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn store() -> PromptStore {
        PromptStore::load(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .expect("workspace root"),
        )
        .expect("bundled prompt store")
    }

    #[test]
    fn renders_both_declared_prompt_ids_and_schema() {
        let store = store();
        let mut vars = BTreeMap::new();
        vars.insert("persona".into(), Value::String("P".into()));
        vars.insert("runtime_state".into(), Value::String("R".into()));
        vars.insert("recent_responses".into(), Value::String("[]".into()));
        vars.insert("audio_observations".into(), Value::String("A".into()));
        let prompt = store
            .render(PromptId::Observation, &vars)
            .expect("observation prompt");
        assert!(prompt.user.contains("P"));
        vars.insert("conversation_history".into(), Value::String("[]".into()));
        vars.insert("user_message".into(), Value::String("hello".into()));
        assert_eq!(
            store
                .render(PromptId::DirectChat, &vars)
                .expect("direct prompt")
                .id,
            PromptId::DirectChat
        );
    }

    #[test]
    fn missing_variable_and_path_are_rejected_without_fallback() {
        let store = store();
        let vars = BTreeMap::new();
        assert!(store.render(PromptId::Observation, &vars).is_err());
        let directory = tempdir().expect("temp dir");
        fs::create_dir_all(directory.path().join("resources/prompts")).expect("prompt dir");
        fs::write(directory.path().join("resources/prompts/manifest.toml"), "schema_version=1\n[[prompt]]\nid='observation'\nsystem='../outside'\nuser='x'\nschema='x'\nrequired_variables=['x']\nmax_output_tokens=1\nsampling_profile='x'").expect("manifest");
        assert!(PromptStore::load(directory.path()).is_err());
    }
}
