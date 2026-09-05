import { getActiveWindowKind } from "./lib/window-label";
import { ControlWindow } from "./windows/ControlWindow";
import { DanmakuWindow } from "./windows/DanmakuWindow";
import { PetWindow } from "./windows/PetWindow";

export function WindowRoot() {
  switch (getActiveWindowKind()) {
    case "pet":
      return <PetWindow />;
    case "danmaku":
      return <DanmakuWindow />;
    case "control":
      return <ControlWindow />;
  }
}
