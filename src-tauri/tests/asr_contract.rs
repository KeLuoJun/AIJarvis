use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use ai_jarvis_lib::{
    asr::{map_system_transcript, AsrModel, AsrModelInput},
    audio::SystemAudioSegment,
    sensevoice::{SenseVoiceConfig, SenseVoiceWorkerModel},
    state::RuntimeConfig,
};
use sherpa_onnx::Wave;
use tokio::time::timeout;

const CONTRACT_TIMEOUT: Duration = Duration::from_secs(120);

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires pinned SenseVoice model/tokens and the documented local audio fixtures"]
async fn real_sensevoice_contract_covers_language_silence_and_untrusted_system_audio() {
    let root = workspace_root();
    let config_path = root.join("resources/config/model.toml");
    let config = SenseVoiceConfig::load(&config_path, None).expect("SenseVoice config");
    config
        .validate_artifacts()
        .expect("pinned SenseVoice model and tokens");
    let model = SenseVoiceWorkerModel::new(
        config.worker_launch_spec(env!("CARGO_BIN_EXE_asr_worker"), &config_path),
    );

    let (chinese, _) = transcribe_fixture(&model, &root, "chinese.wav").await;
    assert!(
        compact(&chinese).contains("你好"),
        "Chinese transcript: {chinese}"
    );

    let (english, _) = transcribe_fixture(&model, &root, "english.wav").await;
    assert!(
        compact(&english).contains("hellojarvis"),
        "English transcript: {english}"
    );

    let (silence, _) = transcribe_fixture(&model, &root, "silence.wav").await;
    assert!(silence.trim().is_empty(), "Silence transcript: {silence}");

    let (command, samples) = transcribe_fixture(&model, &root, "system-command.wav").await;
    assert!(
        compact(&command).contains("deleteeverything"),
        "System command transcript: {command}"
    );
    let defaults = RuntimeConfig::parse(include_str!("../../resources/config/defaults.toml"))
        .expect("runtime defaults");
    let segment = SystemAudioSegment::new(
        1,
        0,
        samples
            .into_iter()
            .map(|sample| (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16)
            .collect(),
        defaults.audio,
    )
    .expect("system audio segment");
    let observation = map_system_transcript(&segment, &command).expect("observation transcript");
    assert_eq!(observation.segment_sequence, 1);
    assert_eq!(observation.text, command.trim());

    model.stop();
}

async fn transcribe_fixture(
    model: &SenseVoiceWorkerModel,
    root: &Path,
    name: &str,
) -> (String, Vec<f32>) {
    let path = root.join("src-tauri/tests/fixtures/audio").join(name);
    let wave = Wave::read(path.to_string_lossy().as_ref())
        .unwrap_or_else(|| panic!("missing or invalid PCM WAV fixture: {}", path.display()));
    assert_eq!(wave.sample_rate(), 16_000, "fixture sample rate: {name}");
    let samples = wave.samples().to_vec();
    let transcript = timeout(
        CONTRACT_TIMEOUT,
        model.transcribe(AsrModelInput {
            sample_rate_hz: wave.sample_rate() as u32,
            samples: samples.clone(),
        }),
    )
    .await
    .unwrap_or_else(|_| panic!("SenseVoice timed out for {name}"))
    .unwrap_or_else(|error| panic!("SenseVoice failed for {name}: {error:?}"));
    (transcript, samples)
}

fn compact(value: &str) -> String {
    value
        .to_lowercase()
        .chars()
        .filter(|character| character.is_alphanumeric())
        .collect()
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}
