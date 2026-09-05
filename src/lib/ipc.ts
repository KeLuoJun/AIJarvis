import { invoke } from "@tauri-apps/api/core";

import type { AppWindowKind, BootstrapState } from "./types";

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

export function describeIpcError(error: unknown): string {
  if (error instanceof Error && error.message.trim()) {
    return error.message;
  }
  if (typeof error === "string" && error.trim()) {
    return error;
  }
  return "本地服务没有返回可用信息。";
}
