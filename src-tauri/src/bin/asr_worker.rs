use std::{env, io, path::PathBuf, process::ExitCode};

use ai_jarvis_lib::{
    asr_worker_protocol::{read_request, write_response, WorkerResponse, WorkerStatus},
    sensevoice::{clean_sensevoice_text, SenseVoiceConfig},
};
use sherpa_onnx::{OfflineRecognizer, OfflineRecognizerConfig, OfflineSenseVoiceModelConfig};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => ExitCode::FAILURE,
    }
}

fn run() -> Result<(), ()> {
    let arguments = Arguments::parse(env::args_os().skip(1)).ok_or(())?;
    let config = SenseVoiceConfig::load(&arguments.config, arguments.model_root.as_deref())
        .map_err(|_| ())?;
    config.validate_artifacts().map_err(|_| ())?;
    let recognizer = create_recognizer(&config).ok_or(())?;
    serve(&recognizer)
}

fn create_recognizer(config: &SenseVoiceConfig) -> Option<OfflineRecognizer> {
    let mut recognizer = OfflineRecognizerConfig::default();
    recognizer.feat_config.sample_rate = config.sample_rate_hz as i32;
    recognizer.feat_config.feature_dim = config.feature_dim as i32;
    recognizer.model_config.tokens = Some(config.tokens_path.to_string_lossy().into_owned());
    recognizer.model_config.sense_voice = OfflineSenseVoiceModelConfig {
        model: Some(config.model_path.to_string_lossy().into_owned()),
        language: Some(config.language.clone()),
        use_itn: config.use_itn,
    };
    OfflineRecognizer::create(&recognizer)
}

fn serve(recognizer: &OfflineRecognizer) -> Result<(), ()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    loop {
        let Some(request) = read_request(&mut input).map_err(|_| ())? else {
            return Ok(());
        };
        let stream = recognizer.create_stream();
        stream.accept_waveform(request.sample_rate_hz as i32, &request.samples);
        recognizer.decode(&stream);
        let response = match stream.get_result() {
            Some(result) => WorkerResponse {
                request_id: request.request_id,
                status: WorkerStatus::Success,
                transcript: clean_sensevoice_text(&result.text),
            },
            None => WorkerResponse {
                request_id: request.request_id,
                status: WorkerStatus::InferenceFailed,
                transcript: String::new(),
            },
        };
        write_response(&mut output, &response).map_err(|_| ())?;
    }
}

#[derive(Debug, Eq, PartialEq)]
struct Arguments {
    config: PathBuf,
    model_root: Option<PathBuf>,
}

impl Arguments {
    fn parse(values: impl IntoIterator<Item = std::ffi::OsString>) -> Option<Self> {
        let mut values = values.into_iter();
        let mut config = None;
        let mut model_root = None;
        while let Some(flag) = values.next() {
            let value = values.next()?;
            match flag.to_str()? {
                "--config" if config.is_none() => config = Some(PathBuf::from(value)),
                "--model-root" if model_root.is_none() => model_root = Some(PathBuf::from(value)),
                _ => return None,
            }
        }
        Some(Self {
            config: config?,
            model_root,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_preserve_path_boundaries_and_reject_unknown_flags() {
        let parsed = Arguments::parse([
            "--config".into(),
            "C:\\A path & config\\model.toml".into(),
            "--model-root".into(),
            "C:\\Models & data".into(),
        ])
        .expect("arguments");
        assert_eq!(
            parsed.config,
            PathBuf::from("C:\\A path & config\\model.toml")
        );
        assert_eq!(parsed.model_root, Some(PathBuf::from("C:\\Models & data")));
        assert!(
            Arguments::parse(["--config".into(), "model.toml".into(), "--verbose".into()])
                .is_none()
        );
    }
}
