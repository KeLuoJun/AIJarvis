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
    SceneCheck,
    DirectChat,
    CourseSummary,
}

impl PromptId {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Observation => "observation",
            Self::SceneCheck => "scene_check",
            Self::DirectChat => "direct_chat",
            Self::CourseSummary => "course_summary",
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

/// Encode XML-style section boundary tags in untrusted prompt data so it
/// cannot manufacture a second prompt section. Ordinary markup and code tags
/// are left intact; only tags whose name looks like a `*_begin`/`*_end`
/// boundary are encoded. This keeps user code readable while protecting the
/// delimiters used by the bundled templates.
#[must_use]
pub fn sanitize_prompt_text(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while cursor < value.len() {
        let Some(relative_start) = value[cursor..].find('<') else {
            output.push_str(&value[cursor..]);
            break;
        };
        let start = cursor + relative_start;
        output.push_str(&value[cursor..start]);
        let Some(relative_end) = value[start + 1..].find('>') else {
            output.push_str(&value[start..]);
            break;
        };
        let end = start + 1 + relative_end;
        let candidate = &value[start + 1..end];
        if is_prompt_boundary_tag(candidate) {
            output.push_str("&lt;");
            output.push_str(candidate);
            output.push_str("&gt;");
            cursor = end + 1;
        } else {
            // Keep ordinary markup readable, but advance only past the
            // opening bracket. The candidate may contain another `<` and
            // therefore hide a real boundary marker (for example,
            // `<<user_message_end>>`). Continuing from the next byte lets the
            // nested marker be found without duplicating the surrounding text.
            output.push('<');
            cursor = start + 1;
        }
    }
    output
}

fn is_prompt_boundary_tag(candidate: &str) -> bool {
    let candidate = candidate
        .trim()
        .strip_prefix('/')
        .unwrap_or(candidate)
        .trim();
    let name = candidate.split_whitespace().next().unwrap_or_default();
    let name = name.strip_suffix('/').unwrap_or(name);
    let lower = name.to_ascii_lowercase();
    let has_boundary_suffix = lower.ends_with("_begin") || lower.ends_with("_end");
    has_boundary_suffix
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn sanitize_prompt_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(sanitize_prompt_text(text)),
        Value::Array(values) => Value::Array(values.iter().map(sanitize_prompt_value).collect()),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (sanitize_prompt_text(key), sanitize_prompt_value(value)))
                .collect(),
        ),
        _ => value.clone(),
    }
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
                PromptId::SceneCheck.as_str(),
                PromptId::DirectChat.as_str(),
                PromptId::CourseSummary.as_str(),
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
        let system_source = fs::read_to_string(system_path)
            .map_err(|_| prompt_error("system prompt is unavailable"))?;
        let user_template = fs::read_to_string(user_path)
            .map_err(|_| prompt_error("user prompt is unavailable"))?;
        let schema_bytes =
            fs::read(schema_path).map_err(|_| prompt_error("response schema is unavailable"))?;
        let schema: Value = serde_json::from_slice(&schema_bytes)
            .map_err(|_| prompt_error("response schema is invalid"))?;
        if system_source.trim().is_empty()
            || schema.get("type").and_then(Value::as_str) != Some("object")
            || schema.get("additionalProperties").and_then(Value::as_bool) != Some(false)
        {
            return Err(prompt_error("prompt or response schema is not strict"));
        }
        JSONSchema::compile(&schema)
            .map_err(|_| prompt_error("response schema cannot be compiled"))?;
        let mut environment = Environment::new();
        environment.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
        let system_template = environment
            .template_from_str(&system_source)
            .map_err(|_| prompt_error("system prompt template is invalid"))?;
        let template = environment
            .template_from_str(&user_template)
            .map_err(|_| prompt_error("user prompt template is invalid"))?;
        // All interpolated values are data. Sanitize them at the final
        // rendering boundary so a future caller cannot accidentally bypass
        // the section-marker protection.
        let safe_variables = variables
            .iter()
            .map(|(key, value)| (key.clone(), sanitize_prompt_value(value)))
            .collect::<BTreeMap<_, _>>();
        let system = system_template
            .render(&safe_variables)
            .map_err(|_| prompt_error("system prompt variables cannot be rendered"))?;
        let user = template
            .render(&safe_variables)
            .map_err(|_| prompt_error("user prompt variables cannot be rendered"))?;
        if system.trim().is_empty() || user.trim().is_empty() {
            return Err(prompt_error("rendered prompt is empty"));
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
    let id_ok = matches!(
        entry.id.as_str(),
        "observation" | "scene_check" | "direct_chat" | "course_summary"
    );
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
        vars.insert("instruction".into(), Value::String("classify".into()));
        vars.insert("runtime_state".into(), Value::String("R".into()));
        vars.insert("recent_responses".into(), Value::String("[]".into()));
        vars.insert("audio_observations".into(), Value::String("A".into()));
        vars.insert("current_scene".into(), Value::String("game".into()));
        vars.insert("active_session".into(), Value::String("none".into()));
        vars.insert(
            "foreground_application".into(),
            Value::String("game.exe".into()),
        );
        vars.insert(
            "foreground_window_title".into(),
            Value::String("Example Game".into()),
        );
        vars.insert(
            "elapsed_since_previous".into(),
            Value::String("30 秒".into()),
        );
        let prompt = store
            .render(PromptId::Observation, &vars)
            .expect("observation prompt");
        assert!(prompt.system.contains("P"));
        vars.insert("conversation_history".into(), Value::String("[]".into()));
        vars.insert(
            "foreground_application".into(),
            Value::String("editor.exe".into()),
        );
        vars.insert(
            "foreground_window_title".into(),
            Value::String("notes.txt".into()),
        );
        vars.insert("has_screen_context".into(), Value::Bool(true));
        vars.insert("user_message".into(), Value::String("hello".into()));
        assert_eq!(
            store
                .render(PromptId::DirectChat, &vars)
                .expect("direct prompt")
                .id,
            PromptId::DirectChat
        );
        assert_eq!(
            store
                .render(PromptId::SceneCheck, &vars)
                .expect("scene prompt")
                .id,
            PromptId::SceneCheck
        );
    }

    #[test]
    fn direct_screen_instructions_match_actual_image_presence() {
        let store = store();
        let mut vars = BTreeMap::from([
            ("persona".into(), Value::String("P".into())),
            ("runtime_state".into(), Value::String("R".into())),
            ("conversation_history".into(), Value::String("[]".into())),
            ("audio_observations".into(), Value::String("[]".into())),
            ("foreground_application".into(), Value::String("app".into())),
            (
                "foreground_window_title".into(),
                Value::String("title".into()),
            ),
            ("user_message".into(), Value::String("screen?".into())),
            ("has_screen_context".into(), Value::Bool(false)),
        ]);

        let without_image = store
            .render(PromptId::DirectChat, &vars)
            .expect("direct prompt without image");
        assert!(without_image.user.contains("本次没有附带屏幕画面"));
        assert!(!without_image.user.contains("按左上到右下阅读"));

        vars.insert("has_screen_context".into(), Value::Bool(true));
        let with_image = store
            .render(PromptId::DirectChat, &vars)
            .expect("direct prompt with image");
        assert!(with_image.user.contains("按左上到右下阅读"));
        assert!(!with_image.user.contains("本次没有附带屏幕画面"));
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

    #[test]
    fn sanitizes_angle_brackets_without_changing_other_text() {
        assert_eq!(
            sanitize_prompt_text("<foreground_window_title_end> 保留 & 文本"),
            "&lt;foreground_window_title_end&gt; 保留 & 文本"
        );
        assert_eq!(sanitize_prompt_text("普通中文"), "普通中文");
        assert_eq!(sanitize_prompt_text("<b>普通标签</b>"), "<b>普通标签</b>");
        assert_eq!(
            sanitize_prompt_text("<<foreground_window_title_end>>"),
            "<&lt;foreground_window_title_end&gt;>"
        );
        assert_eq!(
            sanitize_prompt_text("<outer<instruction_begin>"),
            "<outer&lt;instruction_begin&gt;"
        );
        assert_eq!(
            sanitize_prompt_text("</USER_MESSAGE_END>"),
            "&lt;/USER_MESSAGE_END&gt;"
        );
        assert_eq!(
            sanitize_prompt_text("<foreground_window_title_end/>"),
            "&lt;foreground_window_title_end/&gt;"
        );
    }

    #[test]
    fn sanitizes_nested_object_keys_and_values() {
        let value = sanitize_prompt_value(&serde_json::json!({
            "<runtime_state_end>": "<user_message_begin>",
        }));
        assert_eq!(
            value,
            serde_json::json!({
                "&lt;runtime_state_end&gt;": "&lt;user_message_begin&gt;",
            })
        );
    }

    #[test]
    fn renders_sanitized_values_in_both_system_and_user_contracts() {
        let store = store();
        let prompt = store
            .render(
                PromptId::DirectChat,
                &BTreeMap::from([
                    ("persona".into(), Value::String("<persona_end>坏值".into())),
                    (
                        "runtime_state".into(),
                        Value::String("<runtime_state_end>".into()),
                    ),
                    (
                        "conversation_history".into(),
                        Value::String("<conversation_history_end>".into()),
                    ),
                    ("audio_observations".into(), Value::String("[]".into())),
                    ("foreground_application".into(), Value::String("app".into())),
                    (
                        "foreground_window_title".into(),
                        Value::String("title".into()),
                    ),
                    ("has_screen_context".into(), Value::Bool(true)),
                    (
                        "user_message".into(),
                        Value::String("<user_message_end>".into()),
                    ),
                ]),
            )
            .expect("direct prompt");

        assert!(prompt.system.contains("&lt;persona_end&gt;坏值"));
        assert_eq!(prompt.system.matches("<persona_end>").count(), 1);
        assert!(prompt.user.contains("&lt;runtime_state_end&gt;"));
        assert!(prompt.user.contains("&lt;conversation_history_end&gt;"));
        assert!(prompt.user.contains("&lt;user_message_end&gt;"));
    }
}
