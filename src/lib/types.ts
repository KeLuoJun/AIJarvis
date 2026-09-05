export const appWindowKinds = ["control", "pet", "danmaku"] as const;

export type AppWindowKind = (typeof appWindowKinds)[number];

export interface ResourceAvailability {
  modelConfig: boolean;
  defaultsConfig: boolean;
  petManifest: boolean;
}

export interface AppWindowState {
  kind: AppWindowKind;
  open: boolean;
}

export interface BootstrapState {
  appVersion: string;
  currentWindow: AppWindowKind;
  resourceStatus: ResourceAvailability;
  windows: AppWindowState[];
}

export const runtimeFeatures = [
  "screen",
  "systemAudio",
  "proactiveResponse",
] as const;

export type RuntimeFeature = (typeof runtimeFeatures)[number];
export type PrivacyState = "active" | "paused";
export type LifecycleState = "running" | "shuttingDown" | "stopped";

export interface FeatureSettings {
  screenEnabled: boolean;
  systemAudioEnabled: boolean;
  proactiveResponseEnabled: boolean;
}

export type ConfigSource = "startupState" | "settings";
export type ConfigDiagnosticKind =
  | "missing"
  | "invalid"
  | "readFailed"
  | "writeFailed";

export interface ConfigDiagnostic {
  source: ConfigSource;
  kind: ConfigDiagnosticKind;
  message: string;
}

export interface RuntimeSnapshot {
  revision: number;
  privacyState: PrivacyState;
  lifecycleState: LifecycleState;
  features: FeatureSettings;
  diagnostics: ConfigDiagnostic[];
}

export interface ClearTransientOutputEvent {
  revision: number;
  reason: "privacyPaused" | "screenDisabled" | "systemAudioDisabled" | "proactiveResponseDisabled" | "shutdown";
}

export function isAppWindowKind(value: string): value is AppWindowKind {
  return appWindowKinds.includes(value as AppWindowKind);
}
