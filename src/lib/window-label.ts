import { getCurrentWindow } from "@tauri-apps/api/window";

import { isAppWindowKind, type AppWindowKind } from "./types";

export function selectWindowKind(label: string, search = ""): AppWindowKind {
  if (isAppWindowKind(label)) {
    return label;
  }

  const preview = new URLSearchParams(search).get("window");
  return preview && isAppWindowKind(preview) ? preview : "control";
}

export function getActiveWindowKind(): AppWindowKind {
  let label = "";
  try {
    label = getCurrentWindow().label;
  } catch {
    // Browser preview has no Tauri runtime.
  }
  return selectWindowKind(label, window.location.search);
}
