import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

import type {
  ActivitySnapshot,
  AppWindowKind,
  BootstrapState,
  BootstrapProgress,
  ChatAccepted,
  ChatStatusEvent,
  ChatTurn,
  ClearTransientOutputEvent,
  ObservationResultEvent,
  PersonaState,
  RuntimeFeature,
  RuntimeSnapshot,
  VoiceInputState,
} from "./types";

export const runtimeStateChangedEvent = "runtime-state-changed";
export const clearTransientOutputEvent = "clear-transient-output";
export const chatStatusEvent = "chat-status";
export const observationResultEvent = "observation-result";
export const voiceInputStatusEvent = "voice-input-status";
export const bootstrapProgressEvent = "bootstrap-progress";

export function getActivityState(): Promise<ActivitySnapshot> {
  return invoke<ActivitySnapshot>("get_activity_state");
}

export function noteUserActivity(): Promise<void> {
  return invoke<void>("note_user_activity");
}

const ipcErrorCopy: Record<string, string> = {
  invalidArgument: "请求参数无效。",
  configRead: "无法读取本地配置。",
  configWrite: "无法保存本地配置，请重试。",
  generationExhausted: "运行时状态计数已耗尽，请重启 AI Jarvis。",
  queueFull: "当前消息较多，请稍后再试。",
  queueClosed: "本地任务队列已关闭。",
  shuttingDown: "AI Jarvis 正在退出。",
  privacyPaused: "隐私暂停期间无法执行该操作。",
  featureDisabled: "该功能当前已关闭。",
  internal: "本地运行时暂时不可用。",
};

const unknownIpcError = "本地服务没有返回可用信息。";

export function getBootstrapState(): Promise<BootstrapState> {
  return invoke<BootstrapState>("get_bootstrap_state");
}

export function summonAiJarvis(): Promise<void> {
  return invoke<void>("summon_ai_jarvis");
}

export function listenBootstrapProgress(
  handler: (progress: BootstrapProgress) => void,
): Promise<UnlistenFn> {
  return listen<BootstrapProgress>(bootstrapProgressEvent, (event) => {
    handler(event.payload);
  });
}

export function showAppWindow(kind: AppWindowKind): Promise<void> {
  return invoke<void>("show_app_window", { kind });
}

export function closeAppWindow(kind: AppWindowKind): Promise<void> {
  return invoke<void>("close_app_window", { kind });
}

export function startCurrentWindowDragging(): Promise<void> {
  return getCurrentWindow().startDragging();
}

export function quitApp(): Promise<void> {
  return invoke<void>("quit_app");
}

export function getRuntimeState(): Promise<RuntimeSnapshot> {
  return invoke<RuntimeSnapshot>("get_runtime_state");
}

export function getDanmakuState(): Promise<ObservationResultEvent | null> {
  return invoke<ObservationResultEvent | null>("get_danmaku_state");
}

export function setPrivacyPaused(paused: boolean): Promise<RuntimeSnapshot> {
  return invoke<RuntimeSnapshot>("set_privacy_paused", { paused });
}

export function setFeatureEnabled(
  feature: RuntimeFeature,
  enabled: boolean,
): Promise<RuntimeSnapshot> {
  return invoke<RuntimeSnapshot>("set_feature_enabled", { feature, enabled });
}

export function sendTextMessage(message: string): Promise<ChatAccepted> {
  return invoke<ChatAccepted>("send_text_message", { message });
}

export function getChatHistory(): Promise<ChatTurn[]> {
  return invoke<ChatTurn[]>("get_chat_history");
}

export function clearChatHistory(): Promise<void> {
  return invoke<void>("clear_chat_history");
}

export function getPersonas(): Promise<PersonaState> {
  return invoke<PersonaState>("get_personas");
}

export function createPersona(name: string, prompt: string): Promise<PersonaState> {
  return invoke<PersonaState>("create_persona", { name, prompt });
}

export function updatePersona(
  id: string,
  name: string,
  prompt: string,
): Promise<PersonaState> {
  return invoke<PersonaState>("update_persona", { id, name, prompt });
}

export function selectPersona(id: string): Promise<PersonaState> {
  return invoke<PersonaState>("select_persona", { id });
}

export function deletePersona(id: string): Promise<PersonaState> {
  return invoke<PersonaState>("delete_persona", { id });
}

export function getVoiceInputState(): Promise<VoiceInputState> {
  return invoke<VoiceInputState>("get_voice_input_state");
}

export function startVoiceInput(): Promise<VoiceInputState> {
  return invoke<VoiceInputState>("start_voice_input");
}

export function stopVoiceInput(): Promise<VoiceInputState> {
  return invoke<VoiceInputState>("stop_voice_input");
}

export function cancelVoiceInput(): Promise<VoiceInputState> {
  return invoke<VoiceInputState>("cancel_voice_input");
}

export function listenRuntimeState(
  handler: (state: RuntimeSnapshot) => void,
): Promise<UnlistenFn> {
  return listen<RuntimeSnapshot>(runtimeStateChangedEvent, (event) => {
    handler(event.payload);
  });
}

export function listenClearTransientOutput(
  handler: (event: ClearTransientOutputEvent) => void,
): Promise<UnlistenFn> {
  return listen<ClearTransientOutputEvent>(clearTransientOutputEvent, (event) => {
    handler(event.payload);
  });
}

export function listenChatStatus(
  handler: (event: ChatStatusEvent) => void,
): Promise<UnlistenFn> {
  return listen<ChatStatusEvent>(chatStatusEvent, (event) => {
    handler(event.payload);
  });
}

export function listenObservationResult(
  handler: (event: ObservationResultEvent) => void,
): Promise<UnlistenFn> {
  return listen<ObservationResultEvent>(observationResultEvent, (event) => {
    handler(event.payload);
  });
}

export function listenVoiceInputStatus(
  handler: (state: VoiceInputState) => void,
): Promise<UnlistenFn> {
  return listen<VoiceInputState>(voiceInputStatusEvent, (event) => {
    handler(event.payload);
  });
}

export function describeIpcError(error: unknown): string {
  if (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    typeof error.code === "string"
  ) {
    return ipcErrorCopy[error.code] ?? unknownIpcError;
  }
  return unknownIpcError;
}
