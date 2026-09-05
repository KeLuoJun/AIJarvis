use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};

use crate::types::{ConfigSource, Feature};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    InvalidArgument,
    ConfigRead,
    ConfigWrite,
    GenerationExhausted,
    QueueFull,
    QueueClosed,
    ShuttingDown,
    PrivacyPaused,
    FeatureDisabled,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppError {
    pub code: ErrorCode,
    pub message: String,
}

impl AppError {
    pub(crate) fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }

    pub fn queue_full() -> Self {
        Self::new(ErrorCode::QueueFull, "The direct model queue is full.")
    }

    pub fn config_read(source: ConfigSource) -> Self {
        Self::new(
            ErrorCode::ConfigRead,
            format!("Could not read the {} configuration.", source.label()),
        )
    }

    pub fn config_write(source: ConfigSource) -> Self {
        Self::new(
            ErrorCode::ConfigWrite,
            format!("Could not write the {} configuration.", source.label()),
        )
    }

    pub(crate) fn generation_exhausted() -> Self {
        Self::new(
            ErrorCode::GenerationExhausted,
            "The runtime generation counter is exhausted.",
        )
    }

    pub(crate) fn shutting_down() -> Self {
        Self::new(ErrorCode::ShuttingDown, "The application is shutting down.")
    }

    pub(crate) fn privacy_paused() -> Self {
        Self::new(
            ErrorCode::PrivacyPaused,
            "Background work is disabled while privacy mode is paused.",
        )
    }

    pub(crate) fn feature_disabled(feature: Feature) -> Self {
        let feature = match feature {
            Feature::Screen => "screen capture",
            Feature::SystemAudio => "system audio capture",
            Feature::ProactiveResponse => "proactive response",
        };
        Self::new(
            ErrorCode::FeatureDisabled,
            format!("The {feature} feature is disabled."),
        )
    }
}

impl ConfigSource {
    const fn label(self) -> &'static str {
        match self {
            Self::StartupState => "startup state",
            Self::Settings => "settings",
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for AppError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_error_serializes_as_a_stable_object() {
        assert_eq!(
            serde_json::to_value(AppError::config_write(ConfigSource::Settings))
                .expect("serialize application error"),
            serde_json::json!({
                "code": "configWrite",
                "message": "Could not write the settings configuration."
            })
        );
    }
}
