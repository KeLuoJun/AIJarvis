import { PanelTopOpen, X } from "lucide-react";
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type MouseEvent as ReactMouseEvent,
} from "react";

import { PetSprite } from "../components/PetSprite";
import { MessageInput } from "../components/MessageInput";
import { SpeechBubble } from "../components/SpeechBubble";
import { useChatSession } from "../hooks/useChatSession";
import { useActivityState } from "../hooks/useActivityState";
import {
  closeAppWindow,
  describeIpcError,
  getRuntimeState,
  listenClearTransientOutput,
  listenObservationResult,
  listenRuntimeState,
  setPrivacyPaused,
  showAppWindow,
  startCurrentWindowDragging,
} from "../lib/ipc";
import type { ObservedScene, PrivacyState, RuntimeSnapshot } from "../lib/types";

const pauseAnimationDurationMs = 2_500;
const interactionAnimationDurationMs = 2_500;
const bubbleDisplayDurationMs = 10_000;
const reducedMotionQuery = "(prefers-reduced-motion: reduce)";

interface PetInteraction {
  key: string;
  source: "direct" | "proactive";
}

export function PetWindow() {
  const [privacyState, setPrivacyState] = useState<PrivacyState>();
  const [runtimeGeneration, setRuntimeGeneration] = useState<number>();
  const [runtimeRevision, setRuntimeRevision] = useState<number>();
  const [pauseAnimated, setPauseAnimated] = useState(false);
  const [pauseReplayKey, setPauseReplayKey] = useState(0);
  const [interaction, setInteraction] = useState<PetInteraction>();
  const [idleStatic, setIdleStatic] = useState(false);
  const [presenceReplay, setPresenceReplay] = useState(0);
  const activity = useActivityState(privacyState === "active");
  const [transientClearRevision, setTransientClearRevision] = useState(0);
  const [runtimeError, setRuntimeError] = useState<string>();
  const [actionError, setActionError] = useState<string>();
  const [inputVisible, setInputVisible] = useState(false);
  const [bubbleVisible, setBubbleVisible] = useState(false);
  const [observedScene, setObservedScene] = useState<ObservedScene>();
  const [reducedMotion, setReducedMotion] = useState(
    () => window.matchMedia?.(reducedMotionQuery).matches ?? false,
  );
  const privacyStateRef = useRef<PrivacyState | undefined>(undefined);
  const runtimeGenerationRef = useRef(-1);
  const runtimeRevisionRef = useRef(-1);
  const pauseTimerRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const interactionTimerRef = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);
  const interactionSourceRef = useRef<PetInteraction["source"] | undefined>(undefined);
  const lastInteractionKeyRef = useRef<string | undefined>(undefined);
  const reducedMotionRef = useRef(reducedMotion);
  const privacyTogglePendingRef = useRef(false);
  const chat = useChatSession(runtimeGeneration, runtimeRevision);
  const lastObservedCheck = useRef(0);
  const presenceAllowed = privacyState === "active" && !reducedMotion && !interaction && activity?.mode !== "protected";

  useEffect(() => {
    if (!presenceAllowed) return;
    const timer = setInterval(() => {
      setPresenceReplay((current) => current + 1);
      setIdleStatic(false);
    }, 15_000);
    return () => clearInterval(timer);
  }, [presenceAllowed]);

  useEffect(() => {
    const completed = activity?.successfulChecks ?? 0;
    if (completed === lastObservedCheck.current) return;
    lastObservedCheck.current = completed;
    if (!presenceAllowed || !completed) return;
    setPresenceReplay((current) => current + 1);
    setIdleStatic(false);
  }, [activity?.successfulChecks, presenceAllowed]);

  useEffect(() => {
    if (privacyState !== "active" || reducedMotion) return;
    setPresenceReplay((current) => current + 1);
    setIdleStatic(false);
  }, [privacyState, reducedMotion]);

  useEffect(() => {
    if (presenceReplay === 0) return;
    const timer = setTimeout(() => setIdleStatic(true), interactionAnimationDurationMs);
    return () => clearTimeout(timer);
  }, [presenceReplay]);

  const cancelInteraction = useCallback((settleToIdle = true) => {
    if (interactionTimerRef.current) clearTimeout(interactionTimerRef.current);
    interactionTimerRef.current = undefined;
    interactionSourceRef.current = undefined;
    setInteraction(undefined);
    if (settleToIdle) setIdleStatic(true);
  }, []);

  const playInteraction = useCallback((next: PetInteraction) => {
    if (
      privacyStateRef.current !== "active" ||
      lastInteractionKeyRef.current === next.key
    ) {
      return;
    }
    lastInteractionKeyRef.current = next.key;
    if (interactionTimerRef.current) clearTimeout(interactionTimerRef.current);
    interactionSourceRef.current = next.source;
    setIdleStatic(false);
    setInteraction(next);
    interactionTimerRef.current = setTimeout(() => {
      interactionTimerRef.current = undefined;
      interactionSourceRef.current = undefined;
      setInteraction(undefined);
      setIdleStatic(true);
    }, interactionAnimationDurationMs);
  }, []);

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
    const toggleInput = (event: KeyboardEvent) => {
      if (!event.ctrlKey || event.repeat || event.key.toLowerCase() !== "m") {
        return;
      }
      event.preventDefault();
      setInputVisible((visible) => !visible);
    };

    window.addEventListener("keydown", toggleInput);
    return () => window.removeEventListener("keydown", toggleInput);
  }, []);

  useEffect(() => {
    let active = true;
    const unlisteners: Array<() => void> = [];

    const applySnapshot = (snapshot: RuntimeSnapshot, animate: boolean) => {
      if (!active || snapshot.revision <= runtimeRevisionRef.current) return;
      const enteringPause = animate && snapshot.privacyState === "paused" && privacyStateRef.current !== "paused";
      if (snapshot.runtimeGeneration > runtimeGenerationRef.current) {
        setObservedScene(undefined);
      }
      runtimeRevisionRef.current = snapshot.revision;
      runtimeGenerationRef.current = snapshot.runtimeGeneration;
      setRuntimeGeneration(snapshot.runtimeGeneration);
      setRuntimeRevision(snapshot.revision);
      privacyStateRef.current = snapshot.privacyState;
      setPrivacyState(snapshot.privacyState);
      setRuntimeError(undefined);

      if (enteringPause) {
        cancelInteraction(false);
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
          if (!active) return;
          setObservedScene("other");
          setTransientClearRevision((current) => Math.max(current, event.revision));
          if (
            event.reason === "privacyPaused" ||
            event.reason === "shutdown" ||
            interactionSourceRef.current === "proactive"
          ) {
            cancelInteraction();
          }
        });
        if (!active) { unlistenClear(); return; }
        unlisteners.push(unlistenClear);

        const unlistenObservation = await listenObservationResult((event) => {
          if (
            !active ||
            event.runtimeGeneration < runtimeGenerationRef.current ||
            event.runtimeRevision < runtimeRevisionRef.current
          ) {
            return;
          }
          setObservedScene(event.scene);
          if (
            event.scene !== "game" ||
            !event.needResponse ||
            !event.message.trim()
          ) {
            return;
          }
          playInteraction({
            key: `game-${event.runtimeGeneration}-${event.runtimeRevision}-${event.latestFrameSequence}`,
            source: "proactive",
          });
        });
        if (!active) { unlistenObservation(); return; }
        unlisteners.push(unlistenObservation);
        applySnapshot(await getRuntimeState(), false);
      } catch (reason) {
        if (active) setRuntimeError(describeIpcError(reason));
      }
    };

    void connect();
    return () => {
      active = false;
      if (pauseTimerRef.current) clearTimeout(pauseTimerRef.current);
      if (interactionTimerRef.current) clearTimeout(interactionTimerRef.current);
      for (const unlisten of unlisteners) unlisten();
    };
  }, [cancelInteraction, playInteraction]);

  const latestExchange = chat.exchanges.at(-1);
  useEffect(() => {
    if (!latestExchange) {
      setBubbleVisible(false);
      return;
    }

    setBubbleVisible(true);
    const dismissTimer = setTimeout(
      () => setBubbleVisible(false),
      bubbleDisplayDurationMs,
    );
    return () => clearTimeout(dismissTimer);
  }, [latestExchange]);

  useEffect(() => {
    if (
      latestExchange?.status === "replied" &&
      latestExchange.reply &&
      !latestExchange.key.startsWith("history-")
    ) {
      playInteraction({
        key: latestExchange.key,
        source: latestExchange.source,
      });
    }
  }, [latestExchange, playInteraction]);

  const run = async (action: () => Promise<void>) => {
    try {
      setActionError(undefined);
      await action();
    } catch (reason) {
      setActionError(describeIpcError(reason));
    }
  };

  const dragPetWindow = (event: ReactMouseEvent<HTMLDivElement>) => {
    if (event.button !== 0 || event.detail > 1) return;
    event.preventDefault();
    void run(startCurrentWindowDragging);
  };

  const togglePrivacy = async (event: ReactMouseEvent<HTMLDivElement>) => {
    event.preventDefault();
    if (privacyStateRef.current === undefined || privacyTogglePendingRef.current) return;
    privacyTogglePendingRef.current = true;
    try {
      await run(async () => {
        await setPrivacyPaused(privacyStateRef.current === "active");
      });
    } finally {
      privacyTogglePendingRef.current = false;
    }
  };

  const paused = privacyState === "paused";
  const visualPaused = privacyState !== "active";
  const visualScene = observedScene ?? activity?.scene;
  const courseActive =
    visualScene === "course" &&
    (observedScene !== undefined || activity?.mode !== "transitioning");
  const visualState = visualPaused
    ? "paused"
    : courseActive
      ? "course"
      : interaction
        ? "normal"
        : "idle";
  const visualAnimated =
    !reducedMotion &&
    (paused ? pauseAnimated : interaction ? true : !idleStatic);
  const visualReplayKey = paused
    ? pauseReplayKey
    : interaction?.key ?? presenceReplay;
  const visibleError = actionError ?? runtimeError;
  const statusCopy = runtimeError
    ? undefined
    : privacyState === undefined
      ? "桌宠正在连接本地运行时。"
      : paused
        ? "桌宠处于隐私暂停状态。"
        : "桌宠处于本地运行状态。";

  return (
    <main className={`pet-window ${paused ? "pet-window--paused" : ""}`} data-window="pet" data-runtime-state={privacyState ?? "loading"} data-transient-clear-revision={transientClearRevision} data-input-visible={String(inputVisible)} aria-busy={privacyState === undefined && !runtimeError}>
      {statusCopy ? (
        <p className="sr-only" role="status" aria-live="polite" aria-atomic="true">
          {statusCopy}
        </p>
      ) : null}
      <div
        className="pet-window__drag"
        data-pet-drag-region
        title="双击切换隐私模式"
        onMouseDown={dragPetWindow}
        onDoubleClick={(event) => void togglePrivacy(event)}
      >
        <PetSprite className="pet-window__sprite" state={visualState} animated={visualAnimated} replayKey={visualReplayKey} />
      </div>
      <div className="pet-chat">
        {bubbleVisible && latestExchange ? (
          <SpeechBubble exchange={latestExchange} compact />
        ) : null}
        {chat.error ? <p className="pet-chat__error" role="alert">{chat.error}</p> : null}
      </div>
      {inputVisible ? (
        <div className="pet-input">
          <MessageInput
            compact
            autoFocus
            busy={chat.submitting}
            disabled={!chat.ready || privacyState !== "active"}
            onSend={chat.send}
          />
        </div>
      ) : null}
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
