use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use ai_jarvis_lib::{
    llama::{parse_chat_response, LlamaConfig, ModelOutput},
    prompts::{PromptId, PromptStore},
    sensevoice::SenseVoiceConfig,
    state::RuntimeConfig,
};
use serde_json::{json, Value};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn variables(id: PromptId) -> BTreeMap<String, Value> {
    let mut values = BTreeMap::from([
        ("persona".into(), json!("test persona")),
        ("runtime_state".into(), json!("test runtime")),
    ]);
    match id {
        PromptId::Observation => {
            values.insert("recent_responses".into(), json!("[]"));
            values.insert("audio_observations".into(), json!("none"));
            values.insert("elapsed_since_previous".into(), json!("30 秒"));
            values.insert("foreground_application".into(), json!("game.exe"));
            values.insert("foreground_window_title".into(), json!("Example Game"));
        }
        PromptId::SceneCheck => {
            values.insert("instruction".into(), json!("classify"));
            values.insert("foreground_application".into(), json!("game.exe"));
            values.insert("foreground_window_title".into(), json!("Example Game"));
        }
        PromptId::DirectChat => {
            values.insert("conversation_history".into(), json!("[]"));
            values.insert("user_message".into(), json!("hello"));
        }
    }
    values
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create copied resource directory");
    for entry in fs::read_dir(source).expect("read resource directory") {
        let entry = entry.expect("resource entry");
        let target = destination.join(entry.file_name());
        if entry.file_type().expect("resource type").is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy resource file");
        }
    }
}

#[test]
fn bundled_manifest_exposes_only_declared_renderable_prompt_ids() {
    let root = workspace_root();
    let store = PromptStore::load(&root).expect("prompt store");
    for id in [
        PromptId::Observation,
        PromptId::SceneCheck,
        PromptId::DirectChat,
    ] {
        let rendered = store.render(id, &variables(id)).expect("rendered prompt");
        assert!(!rendered.system.trim().is_empty());
        assert!(!rendered.user.trim().is_empty());
        assert_eq!(rendered.schema["additionalProperties"], false);
        assert!(rendered.max_output_tokens > 0);
    }

    let manifest =
        fs::read_to_string(root.join("resources/prompts/manifest.toml")).expect("prompt manifest");
    let document = toml::from_str::<toml::Value>(&manifest).expect("valid manifest");
    assert_eq!(document["prompt"].as_array().expect("prompt list").len(), 3);
}

#[test]
fn observation_accepts_silence_but_never_model_scheduling_fields() {
    let prompt = PromptStore::load(workspace_root())
        .unwrap()
        .render(PromptId::Observation, &variables(PromptId::Observation))
        .unwrap();
    let parse = |value: Value| {
        parse_chat_response(
            PromptId::Observation,
            &prompt.schema,
            &json!({"choices": [{"message": {"content": value.to_string()}}]}),
        )
    };
    assert_eq!(
        prompt.schema.pointer("/properties/message"),
        Some(&json!({"type": "string"})),
        "llama.cpp grammar conversion rejects string length keywords"
    );
    assert!(parse(json!({"scene":"other","message":"可以先运行这段代码看看结果。"})).is_ok());
    assert!(parse(json!({"scene":"course","message":""})).is_ok());
    for invalid in [
        json!({"scene":"course"}),
        json!({"scene":"course","message":null}),
        json!({"scene":"other","message":"回复","need_response":false}),
        json!({"scene":"other","message":"回复","next_check_seconds":180}),
    ] {
        assert!(parse(invalid).is_err());
    }
}

#[test]
fn direct_output_fixtures_enforce_one_plain_strict_json_object() {
    let root = workspace_root();
    let prompt = PromptStore::load(&root)
        .expect("prompt store")
        .render(PromptId::DirectChat, &variables(PromptId::DirectChat))
        .expect("direct prompt");
    let fixtures = root.join("src-tauri/tests/fixtures/model_outputs");

    let valid: Value = serde_json::from_slice(
        &fs::read(fixtures.join("direct-valid.json")).expect("valid fixture"),
    )
    .expect("valid fixture JSON");
    assert!(matches!(
        parse_chat_response(PromptId::DirectChat, &prompt.schema, &valid),
        Ok(ModelOutput::DirectChat(_))
    ));

    for name in [
        "direct-extra-field.json",
        "direct-markdown.json",
        "direct-truncated.json",
        "direct-thinking.json",
    ] {
        let fixture: Value = serde_json::from_slice(
            &fs::read(fixtures.join(name)).unwrap_or_else(|_| panic!("fixture {name}")),
        )
        .unwrap_or_else(|_| panic!("fixture JSON {name}"));
        assert!(
            parse_chat_response(PromptId::DirectChat, &prompt.schema, &fixture).is_err(),
            "fixture should be rejected: {name}"
        );
    }
}

#[test]
fn packaged_resource_root_loads_every_contract_and_missing_prompt_fails_closed() {
    let root = workspace_root();
    let packaged = tempfile::tempdir().expect("packaged resource root");
    copy_tree(&root.join("resources"), &packaged.path().join("resources"));

    RuntimeConfig::load(&packaged.path().join("resources/config/defaults.toml"))
        .expect("runtime defaults from resource root");
    LlamaConfig::load(
        &packaged.path().join("resources/config/model.toml"),
        Some(&packaged.path().join("models")),
        None,
    )
    .expect("Qwen config from resource root");
    SenseVoiceConfig::load(
        &packaged.path().join("resources/config/model.toml"),
        Some(&packaged.path().join("models")),
    )
    .expect("SenseVoice config from resource root");

    let store = PromptStore::load(packaged.path()).expect("prompt store from resource root");
    for id in [
        PromptId::Observation,
        PromptId::SceneCheck,
        PromptId::DirectChat,
    ] {
        store
            .render(id, &variables(id))
            .unwrap_or_else(|_| panic!("render packaged prompt {}", id.as_str()));
    }

    fs::remove_file(
        packaged
            .path()
            .join("resources/prompts/observation/system.md"),
    )
    .expect("remove packaged observation prompt");
    assert!(store
        .render(PromptId::Observation, &variables(PromptId::Observation))
        .is_err());
}

#[test]
fn source_code_contains_no_bundled_prompt_body_or_fallback() {
    let root = workspace_root();
    let prompt_files = [
        "resources/prompts/persona/default.md",
        "resources/prompts/observation/system.md",
        "resources/prompts/observation/user.md.jinja",
        "resources/prompts/direct-chat/system.md",
        "resources/prompts/direct-chat/user.md.jinja",
    ];
    let bodies = prompt_files
        .iter()
        .map(|path| {
            fs::read_to_string(root.join(path))
                .unwrap_or_else(|_| panic!("prompt file {path}"))
                .trim()
                .to_owned()
        })
        .collect::<Vec<_>>();

    let mut sources = Vec::new();
    collect_sources(&root.join("src"), &mut sources);
    collect_sources(&root.join("src-tauri/src"), &mut sources);
    collect_sources(&root.join("src-tauri/tests"), &mut sources);
    collect_sources(&root.join("scripts"), &mut sources);
    for source in sources {
        let contents = fs::read_to_string(&source).expect("source text");
        for body in &bodies {
            assert!(
                !contents.contains(body),
                "prompt body duplicated outside resources: {}",
                source.display()
            );
        }
    }
}

fn collect_sources(directory: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_sources(&path, output);
        } else if matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("rs" | "ts" | "tsx" | "ps1" | "psm1")
        ) {
            output.push(path);
        }
    }
}
