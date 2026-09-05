use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PrivacyState {
    Active,
    Paused,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum LifecycleState {
    Running,
    ShuttingDown,
    Stopped,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Feature {
    Screen,
    SystemAudio,
    ProactiveResponse,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeatureSettings {
    pub screen_enabled: bool,
    pub system_audio_enabled: bool,
    pub proactive_response_enabled: bool,
}

impl FeatureSettings {
    pub const fn is_enabled(self, feature: Feature) -> bool {
        match feature {
            Feature::Screen => self.screen_enabled,
            Feature::SystemAudio => self.system_audio_enabled,
            Feature::ProactiveResponse => self.proactive_response_enabled,
        }
    }

    pub const fn with_feature(mut self, feature: Feature, enabled: bool) -> Self {
        match feature {
            Feature::Screen => self.screen_enabled = enabled,
            Feature::SystemAudio => self.system_audio_enabled = enabled,
            Feature::ProactiveResponse => self.proactive_response_enabled = enabled,
        }
        self
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WorkScope {
    Direct,
    ScreenCapture,
    SystemAudioCapture,
    Observation {
        #[serde(rename = "usesScreen")]
        uses_screen: bool,
        #[serde(rename = "usesSystemAudio")]
        uses_system_audio: bool,
    },
}

impl WorkScope {
    pub const fn uses_screen(self) -> bool {
        matches!(self, Self::ScreenCapture)
            || matches!(
                self,
                Self::Observation {
                    uses_screen: true,
                    ..
                }
            )
    }

    pub const fn uses_system_audio(self) -> bool {
        matches!(self, Self::SystemAudioCapture)
            || matches!(
                self,
                Self::Observation {
                    uses_system_audio: true,
                    ..
                }
            )
    }

    pub const fn uses_proactive(self) -> bool {
        matches!(self, Self::Observation { .. })
    }

    pub const fn is_direct(self) -> bool {
        matches!(self, Self::Direct)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConfigSource {
    StartupState,
    Settings,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ConfigDiagnosticKind {
    Missing,
    Invalid,
    ReadFailed,
    WriteFailed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigDiagnostic {
    pub source: ConfigSource,
    pub kind: ConfigDiagnosticKind,
    pub message: String,
}

impl ConfigDiagnostic {
    pub fn new(
        source: ConfigSource,
        kind: ConfigDiagnosticKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            source,
            kind,
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeSnapshot {
    pub revision: u64,
    pub runtime_generation: u64,
    pub privacy_state: PrivacyState,
    pub lifecycle_state: LifecycleState,
    pub features: FeatureSettings,
    pub diagnostics: Vec<ConfigDiagnostic>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ClearTransientOutputReason {
    PrivacyPaused,
    ScreenDisabled,
    SystemAudioDisabled,
    ProactiveResponseDisabled,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearTransientOutputEvent {
    pub revision: u64,
    pub reason: ClearTransientOutputReason,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_snapshot_uses_the_fixed_camel_case_contract() {
        let snapshot = RuntimeSnapshot {
            revision: 7,
            runtime_generation: 11,
            privacy_state: PrivacyState::Paused,
            lifecycle_state: LifecycleState::ShuttingDown,
            features: FeatureSettings {
                screen_enabled: true,
                system_audio_enabled: false,
                proactive_response_enabled: true,
            },
            diagnostics: vec![ConfigDiagnostic::new(
                ConfigSource::StartupState,
                ConfigDiagnosticKind::Invalid,
                "The startup state is invalid.",
            )],
        };

        assert_eq!(
            serde_json::to_value(snapshot).expect("serialize runtime snapshot"),
            serde_json::json!({
                "revision": 7,
                "runtimeGeneration": 11,
                "privacyState": "paused",
                "lifecycleState": "shuttingDown",
                "features": {
                    "screenEnabled": true,
                    "systemAudioEnabled": false,
                    "proactiveResponseEnabled": true
                },
                "diagnostics": [{
                    "source": "startupState",
                    "kind": "invalid",
                    "message": "The startup state is invalid."
                }]
            })
        );
    }

    #[test]
    fn work_scope_reports_only_its_actual_dependencies() {
        assert!(WorkScope::Direct.is_direct());
        assert!(!WorkScope::Direct.uses_screen());
        assert!(!WorkScope::Direct.uses_system_audio());
        assert!(!WorkScope::Direct.uses_proactive());

        let observation = WorkScope::Observation {
            uses_screen: true,
            uses_system_audio: false,
        };
        assert!(observation.uses_screen());
        assert!(!observation.uses_system_audio());
        assert!(observation.uses_proactive());
    }

    #[test]
    fn transient_output_event_uses_the_fixed_camel_case_contract() {
        let event = ClearTransientOutputEvent {
            revision: 11,
            reason: ClearTransientOutputReason::SystemAudioDisabled,
        };

        assert_eq!(
            serde_json::to_value(event).expect("serialize clear event"),
            serde_json::json!({
                "revision": 11,
                "reason": "systemAudioDisabled"
            })
        );
    }
}
