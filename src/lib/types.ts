export const appWindowKinds = ["launcher", "control", "pet", "danmaku"] as const;

export interface ActivitySnapshot {
  epoch: number;
  mode: "available" | "transitioning" | "focused" | "media" | "gaming" | "quiet" | "protected" | "interacting";
  scene: ObservationResultEvent["scene"] | null;
  inputIdleSeconds: number | null;
  regularCheckSeconds: number;
  modelCalls: number;
  suppressedResponses: number;
  successfulChecks: number;
  silentChecks: number;
  failedChecks: number;
  modelBusy: boolean;
  lastCompletedAgoSeconds: number | null;
}

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

export type BootstrapPhase =
  | "checking"
  | "downloading"
  | "verifying"
  | "starting"
  | "complete"
  | "failed";

export interface BootstrapProgress {
  phase: BootstrapPhase;
  step: string;
  label: string;
  detail: string;
  artifactIndex: number;
  artifactCount: number;
  currentBytes: number;
  currentTotalBytes: number;
  completedBytes: number;
  totalBytes: number;
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
  runtimeGeneration: number;
  privacyState: PrivacyState;
  lifecycleState: LifecycleState;
  features: FeatureSettings;
  diagnostics: ConfigDiagnostic[];
}

export interface ClearTransientOutputEvent {
  revision: number;
  runtimeGeneration: number;
  reason: "privacyPaused" | "screenDisabled" | "systemAudioDisabled" | "proactiveResponseDisabled" | "shutdown";
}

export type ChatRequestStatus = "accepted" | "replied" | "failed" | "cancelled";
export type ChatFailureKind =
  | "modelUnavailable"
  | "timeout"
  | "invalidResponse"
  | "queueFull"
  | "internal";

export interface ChatAccepted {
  requestId: number;
  status: "accepted";
}

export interface ChatStatusEvent {
  requestId: number;
  runtimeGeneration: number;
  status: ChatRequestStatus;
  userMessage: string;
  reply: string | null;
  failure: ChatFailureKind | null;
}

export interface ChatTurn {
  user: string;
  assistant: string;
  userTokens: number;
  assistantTokens: number;
}

export type VoiceInputStatus =
  | "idle"
  | "recording"
  | "processing"
  | "submitted"
  | "notHeard"
  | "failed"
  | "cancelled";

export interface VoiceInputState {
  revision: number;
  requestId: number | null;
  status: VoiceInputStatus;
}

export type ObservedScene =
  | "game"
  | "course"
  | "meeting"
  | "video"
  | "coding"
  | "reading"
  | "work"
  | "chat"
  | "other";

export interface ObservationResultEvent {
  runtimeRevision: number;
  runtimeGeneration: number;
  latestFrameSequence: number;
  scene: ObservedScene;
  needResponse: boolean;
  nextCheckSeconds: number;
  message: string;
}

export function isAppWindowKind(value: string): value is AppWindowKind {
  return appWindowKinds.includes(value as AppWindowKind);
}
