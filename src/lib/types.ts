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

export function isAppWindowKind(value: string): value is AppWindowKind {
  return appWindowKinds.includes(value as AppWindowKind);
}
