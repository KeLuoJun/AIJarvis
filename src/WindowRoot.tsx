import { getActiveWindowKind } from "./lib/window-label";
import { ControlWindow } from "./windows/ControlWindow";
import { DanmakuWindow } from "./windows/DanmakuWindow";
import { PetWindow } from "./windows/PetWindow";
import { LauncherWindow } from "./windows/LauncherWindow";

export function WindowRoot() {
  switch (getActiveWindowKind()) {
    case "launcher":
      return <LauncherWindow />;
    case "pet":
      return <PetWindow />;
    case "danmaku":
      return <DanmakuWindow />;
    case "control":
      return <ControlWindow />;
  }
}
