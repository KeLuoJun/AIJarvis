import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type {
  AppWindowKind,
  BootstrapState,
  ClearTransientOutputEvent,
  RuntimeFeature,
  RuntimeSnapshot,
} from "./types";

export const runtimeStateChangedEvent = "runtime-state-changed";
export const clearTransientOutputEvent = "clear-transient-output";

const ipcErrorCopy: Record<string, string> = {
  invalidArgument: "请求参数无效。",
  configRead: "无法读取本地配置。",
  configWrite: "无法保存本地配置，请重试。",
  generationExhausted: "运行时状态计数已耗尽，请重启 AI Jarvis。",
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

export function showAppWindow(kind: AppWindowKind): Promise<void> {
  return invoke<void>("show_app_window", { kind });
}

export function closeAppWindow(kind: AppWindowKind): Promise<void> {
  return invoke<void>("close_app_window", { kind });
}

export function quitApp(): Promise<void> {
  return invoke<void>("quit_app");
}

export function getRuntimeState(): Promise<RuntimeSnapshot> {
  return invoke<RuntimeSnapshot>("get_runtime_state");
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
