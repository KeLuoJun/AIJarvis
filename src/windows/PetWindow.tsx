import { PanelTopOpen, X } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { PetSprite } from "../components/PetSprite";
import { MessageInput } from "../components/MessageInput";
import { SpeechBubble } from "../components/SpeechBubble";
import { useChatSession } from "../hooks/useChatSession";
import {
  closeAppWindow,
  describeIpcError,
  getRuntimeState,
  listenClearTransientOutput,
  listenRuntimeState,
  showAppWindow,
} from "../lib/ipc";
import type { PrivacyState, RuntimeSnapshot } from "../lib/types";

const pauseAnimationDurationMs = 2_500;
const reducedMotionQuery = "(prefers-reduced-motion: reduce)";

export function PetWindow() {
  const [privacyState, setPrivacyState] = useState<PrivacyState>();
  const [runtimeGeneration, setRuntimeGeneration] = useState<number>();
  const [pauseAnimated, setPauseAnimated] = useState(false);
  const [pauseReplayKey, setPauseReplayKey] = useState(0);
  const [transientClearRevision, setTransientClearRevision] = useState(0);
  const [runtimeError, setRuntimeError] = useState<string>();
  const [actionError, setActionError] = useState<string>();
  const [reducedMotion, setReducedMotion] = useState(
    () => window.matchMedia?.(reducedMotionQuery).matches ?? false,
  );
  const privacyStateRef = useRef<PrivacyState | undefined>(undefined);
  const runtimeRevisionRef = useRef(-1);
  const pauseTimerRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const reducedMotionRef = useRef(reducedMotion);
  const chat = useChatSession(runtimeGeneration);

  useEffect(() => {
    const mediaQuery = window.matchMedia?.(reducedMotionQuery);
    if (!mediaQuery) return;

    const applyPreference = (matches: boolean) => {
      reducedMotionRef.current = matches;
      setReducedMotion(matches);
      if (matches) {
        if (pauseTimerRef.current) clearTimeout(pauseTimerRef.current);
        setPauseAnimated(false);
      }
    };
    const onChange = (event: MediaQueryListEvent) => applyPreference(event.matches);
    applyPreference(mediaQuery.matches);
    mediaQuery.addEventListener("change", onChange);
    return () => mediaQuery.removeEventListener("change", onChange);
  }, []);

  useEffect(() => {
    let active = true;
    const unlisteners: Array<() => void> = [];

    const applySnapshot = (snapshot: RuntimeSnapshot, animate: boolean) => {
      if (!active || snapshot.revision <= runtimeRevisionRef.current) return;
      const enteringPause = animate && snapshot.privacyState === "paused" && privacyStateRef.current !== "paused";
      runtimeRevisionRef.current = snapshot.revision;
      setRuntimeGeneration(snapshot.runtimeGeneration);
      privacyStateRef.current = snapshot.privacyState;
      setPrivacyState(snapshot.privacyState);
      setRuntimeError(undefined);

      if (enteringPause) {
        if (pauseTimerRef.current) clearTimeout(pauseTimerRef.current);
        setPauseReplayKey(snapshot.revision);
        if (reducedMotionRef.current) {
          setPauseAnimated(false);
        } else {
          setPauseAnimated(true);
          pauseTimerRef.current = setTimeout(() => {
            if (active) setPauseAnimated(false);
          }, pauseAnimationDurationMs);
        }
      } else if (snapshot.privacyState !== "paused") {
        if (pauseTimerRef.current) clearTimeout(pauseTimerRef.current);
        setPauseAnimated(false);
      }
    };

    const connect = async () => {
      try {
        const unlistenRuntime = await listenRuntimeState((snapshot) => applySnapshot(snapshot, true));
        if (!active) { unlistenRuntime(); return; }
        unlisteners.push(unlistenRuntime);

        const unlistenClear = await listenClearTransientOutput((event) => {
          if (active) setTransientClearRevision((current) => Math.max(current, event.revision));
        });
        if (!active) { unlistenClear(); return; }
        unlisteners.push(unlistenClear);
        applySnapshot(await getRuntimeState(), false);
      } catch (reason) {
        if (active) setRuntimeError(describeIpcError(reason));
      }
    };

    void connect();
    return () => {
      active = false;
      if (pauseTimerRef.current) clearTimeout(pauseTimerRef.current);
      for (const unlisten of unlisteners) unlisten();
    };
  }, []);

  const run = async (action: () => Promise<void>) => {
    try {
      setActionError(undefined);
      await action();
    } catch (reason) {
      setActionError(describeIpcError(reason));
    }
  };

  const paused = privacyState === "paused";
  const visualPaused = privacyState !== "active";
  const visibleError = actionError ?? runtimeError;
  const statusCopy = runtimeError
    ? undefined
    : privacyState === undefined
      ? "桌宠正在连接本地运行时。"
      : paused
        ? "桌宠处于隐私暂停状态。"
        : "桌宠处于本地运行状态。";

  return (
    <main className={`pet-window ${paused ? "pet-window--paused" : ""}`} data-window="pet" data-runtime-state={privacyState ?? "loading"} data-transient-clear-revision={transientClearRevision} aria-busy={privacyState === undefined && !runtimeError}>
      {statusCopy ? (
        <p className="sr-only" role="status" aria-live="polite" aria-atomic="true">
          {statusCopy}
        </p>
      ) : null}
      <div className="pet-window__drag" data-tauri-drag-region>
        <PetSprite className="pet-window__sprite" state={visualPaused ? "paused" : "idle"} animated={!reducedMotion && (paused ? pauseAnimated : !visualPaused)} replayKey={paused ? pauseReplayKey : 0} />
      </div>
      <div className="pet-chat">
        {chat.exchanges.at(-1) ? (
          <SpeechBubble exchange={chat.exchanges.at(-1)!} compact />
        ) : null}
        <MessageInput
          compact
          busy={chat.submitting}
          disabled={!chat.ready || privacyState !== "active"}
          onSend={chat.send}
        />
        {chat.error ? <p className="pet-chat__error" role="alert">{chat.error}</p> : null}
      </div>
      <nav className="pet-tools" aria-label="桌宠窗口">
        <button className="pet-tool" type="button" title="打开控制台" aria-label="打开控制台" onClick={() => void run(() => showAppWindow("control"))}>
          <PanelTopOpen size={18} aria-hidden="true" />
        </button>
        <button className="pet-tool" type="button" title="隐藏桌宠" aria-label="隐藏桌宠" onClick={() => void run(() => closeAppWindow("pet"))}>
          <X size={18} aria-hidden="true" />
        </button>
      </nav>
      {visibleError ? <p className="pet-error" role="alert">{visibleError}</p> : null}
    </main>
  );
}
