import { PanelTopOpen, X } from "lucide-react";
import { useState } from "react";

import { PetSprite } from "../components/PetSprite";
import { closeAppWindow, describeIpcError, showAppWindow } from "../lib/ipc";

export function PetWindow() {
  const [error, setError] = useState<string>();

  const run = async (action: () => Promise<void>) => {
    try {
      setError(undefined);
      await action();
    } catch (reason) {
      setError(describeIpcError(reason));
    }
  };

  return (
    <main className="pet-window" data-window="pet">
      <div className="pet-window__drag" data-tauri-drag-region>
        <PetSprite className="pet-window__sprite" state="idle" animated />
      </div>
      <nav className="pet-tools" aria-label="桌宠窗口">
        <button
          className="pet-tool"
          type="button"
          title="打开控制台"
          aria-label="打开控制台"
          onClick={() => void run(() => showAppWindow("control"))}
        >
          <PanelTopOpen size={18} aria-hidden="true" />
        </button>
        <button
          className="pet-tool"
          type="button"
          title="隐藏桌宠"
          aria-label="隐藏桌宠"
          onClick={() => void run(() => closeAppWindow("pet"))}
        >
          <X size={18} aria-hidden="true" />
        </button>
      </nav>
      {error ? (
        <p className="pet-error" role="alert">
          {error}
        </p>
      ) : null}
    </main>
  );
}
