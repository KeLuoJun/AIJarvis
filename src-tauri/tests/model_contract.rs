use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use ai_jarvis_lib::{
    llama::{
        HttpTransport, LlamaConfig, LlamaError, LlamaService, ModelOutput, ModelRequest,
        ReqwestTransport, SystemProcessSpawner,
    },
    prompts::{PromptId, PromptStore},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use jsonschema::JSONSchema;
use serde_json::{json, Value};

static REAL_MODEL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

#[tokio::test]
#[ignore = "requires pinned Qwen GGUF/mmproj and llama.cpp"]
async fn qwen_real_contract_handles_five_and_eight_ordered_images_without_thinking() {
    assert_ordered_images(&[5, 8]).await;
}

#[tokio::test]
#[ignore = "requires pinned Qwen GGUF/mmproj and llama.cpp"]
async fn qwen_real_activity_contract_handles_one_to_three_ordered_keyframes() {
    assert_ordered_images(&[1, 2, 3]).await;
}

async fn assert_ordered_images(counts: &[usize]) {
    let _model_lock = REAL_MODEL_LOCK.lock().await;
    let root = workspace_root();
    let config = LlamaConfig::load(&root.join("resources/config/model.toml"), None, None)
        .expect("pinned model configuration");
    let prompts = PromptStore::load(&root).expect("prompt resources");
    let service = LlamaService::new(
        config,
        prompts,
        SystemProcessSpawner,
        ReqwestTransport::default(),
    );
    service
        .start(Duration::from_secs(300))
        .await
        .expect("real llama.cpp must become ready");

    let probe: Value = serde_json::from_slice(
        &fs::read(root.join("resources/prompts/probes/qwen-preflight.json"))
            .expect("preflight contract"),
    )
    .expect("preflight JSON");
    let image_paths = probe["image_paths"]
        .as_array()
        .expect("image paths")
        .iter()
        .map(|path| root.join(path.as_str().expect("image path")))
        .collect::<Vec<_>>();
    let expected_poses = probe["expected_poses"].as_array().expect("expected poses");
    let client = reqwest::Client::new();
    for &count in counts {
        let expected = (0..count)
            .map(|index| expected_poses[index % expected_poses.len()].clone())
            .collect::<Vec<_>>();
        let mut schema = probe["vision_response_schema"].clone();
        schema["properties"]["poses"]["minItems"] = json!(count);
        schema["properties"]["poses"]["maxItems"] = json!(count);
        let mut content = vec![json!({
            "type": "text",
            "text": probe["vision_prompt"].as_str().expect("vision prompt")
        })];
        content.extend((0..count).map(|index| {
            json!({
                "type": "image_url",
                "image_url": {"url": png_data_url(&image_paths[index % image_paths.len()])}
            })
        }));
        let response = client
            .post(format!("{}/v1/chat/completions", service.base_url()))
            .timeout(Duration::from_secs(120))
            .json(&json!({
                "model": service.model_alias(),
                "messages": [{"role": "user", "content": content}],
                "response_format": {
                    "type": "json_schema",
                    "json_schema": {"name": "vision_order_contract", "strict": true, "schema": schema.clone()}
                }
            }))
            .send()
            .await
            .unwrap_or_else(|error| panic!("{count}-image request failed: {error}"));
        assert!(response.status().is_success(), "{count}-image HTTP status");
        let envelope: Value = response.json().await.expect("chat response JSON");
        let message = &envelope["choices"][0]["message"];
        assert!(message
            .get("reasoning_content")
            .map_or(true, Value::is_null));
        let content = message["content"].as_str().expect("message content");
        assert!(!content.to_ascii_lowercase().contains("<think>"));
        let output: Value = serde_json::from_str(content).expect("strict JSON object");
        assert!(JSONSchema::compile(&schema)
            .expect("vision schema")
            .is_valid(&output));
        assert_eq!(output["poses"], Value::Array(expected));
    }

    let timeout = ReqwestTransport::default()
        .post(
            "http://127.0.0.1:8080/v1/chat/completions",
            Value::Object(Default::default()),
            Duration::ZERO,
        )
        .await;
    assert!(matches!(timeout, Err(LlamaError::Timeout)));
    service
        .stop(Duration::from_secs(5))
        .await
        .expect("llama.cpp must stop");
}

#[tokio::test]
#[ignore = "requires pinned Qwen GGUF/mmproj and llama.cpp"]
async fn qwen_activity_response_and_cancellation_recovery() {
    let _model_lock = REAL_MODEL_LOCK.lock().await;
    let root = workspace_root();
    let service = LlamaService::new(
        LlamaConfig::load(&root.join("resources/config/model.toml"), None, None).unwrap(),
        PromptStore::load(&root).unwrap(),
        SystemProcessSpawner,
        ReqwestTransport::default(),
    );
    service.start(Duration::from_secs(300)).await.unwrap();
    let probe: Value = serde_json::from_slice(
        &fs::read(root.join("resources/prompts/probes/qwen-preflight.json")).unwrap(),
    )
    .unwrap();
    let image = root.join(probe["image_paths"][0].as_str().unwrap());
    let request = ModelRequest {
        prompt: PromptId::Observation,
        variables: std::collections::BTreeMap::from([
            (
                "persona".into(),
                json!("简洁、体贴、会自己判断是否发声的桌面伙伴。"),
            ),
            ("recent_responses".into(), json!("[]")),
            ("audio_observations".into(), json!("")),
            ("elapsed_since_previous".into(), json!("首次观察")),
            ("current_scene".into(), json!("other")),
            ("active_session".into(), json!("none")),
            ("foreground_application".into(), json!("")),
            ("foreground_window_title".into(), json!("")),
        ]),
        image_data_urls: vec![png_data_url(&image)],
    };
    assert!(tokio::time::timeout(
        Duration::from_millis(10),
        service.request(request.clone(), Duration::from_secs(120))
    )
    .await
    .is_err());
    assert_eq!(service.state(), ai_jarvis_lib::llama::ModelState::Ready);
    let observation = service
        .request(request, Duration::from_secs(120))
        .await
        .expect("request after cancellation");
    let ModelOutput::Observation(observation) = observation else {
        panic!("observation output");
    };
    assert!(observation.message.chars().count() <= 2000);
    service.stop(Duration::from_secs(5)).await.unwrap();
}

fn png_data_url(path: &Path) -> String {
    let bytes = fs::read(path).unwrap_or_else(|_| panic!("image fixture: {}", path.display()));
    format!("data:image/png;base64,{}", STANDARD.encode(bytes))
}
