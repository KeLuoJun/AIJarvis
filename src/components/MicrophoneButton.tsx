import { LoaderCircle, Mic, Square } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";

import {
  cancelVoiceInput,
  describeIpcError,
  getVoiceInputState,
  listenVoiceInputStatus,
  startVoiceInput,
  stopVoiceInput,
} from "../lib/ipc";
import type { VoiceInputState } from "../lib/types";

const initialState: VoiceInputState = {
  revision: -1,
  requestId: null,
  status: "idle",
};

function formatDuration(seconds: number) {
  return `00:${String(Math.min(seconds, 59)).padStart(2, "0")}`;
}

export function MicrophoneButton({ disabled = false }: { disabled?: boolean }) {
  const [voice, setVoice] = useState(initialState);
  const [elapsedSeconds, setElapsedSeconds] = useState(0);
  const [error, setError] = useState<string>();
  const voiceRef = useRef(voice);
  const pending = useRef(false);

  const applyState = useCallback((next: VoiceInputState) => {
    if (next.revision < voiceRef.current.revision) return;
    voiceRef.current = next;
    setVoice(next);
    setError(undefined);
  }, []);

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;
    const connect = async () => {
      try {
        unlisten = await listenVoiceInputStatus((state) => {
          if (active) applyState(state);
        });
        const state = await getVoiceInputState();
        if (active) applyState(state);
      } catch (reason) {
        if (active) setError(describeIpcError(reason));
      }
    };
    void connect();
    return () => {
      active = false;
      unlisten?.();
    };
  }, [applyState]);

  useEffect(() => {
    if (voice.status !== "recording") {
      setElapsedSeconds(0);
      return;
    }
    const startedAt = Date.now();
    const update = () => {
      setElapsedSeconds(Math.floor((Date.now() - startedAt) / 1000));
    };
    update();
    const interval = window.setInterval(update, 250);
    return () => window.clearInterval(interval);
  }, [voice.requestId, voice.status]);

  const run = useCallback(async (action: "start" | "stop" | "cancel") => {
    if (pending.current) return;
    pending.current = true;
    setError(undefined);
    try {
      const next = await (action === "start"
        ? startVoiceInput()
        : action === "stop"
          ? stopVoiceInput()
          : cancelVoiceInput());
      applyState(next);
    } catch (reason) {
      setError(describeIpcError(reason));
    } finally {
      pending.current = false;
    }
  }, [applyState]);

  useEffect(() => {
    const cancelOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || voiceRef.current.status !== "recording") return;
      event.preventDefault();
      void run("cancel");
    };
    window.addEventListener("keydown", cancelOnEscape);
    return () => window.removeEventListener("keydown", cancelOnEscape);
  }, [run]);

  const recording = voice.status === "recording";
  const processing = voice.status === "processing";
  const statusCopy = error
    ?? (recording
      ? `录音中 ${formatDuration(elapsedSeconds)}`
      : voice.status === "processing"
        ? "正在识别"
        : voice.status === "notHeard"
          ? "没听清，再说一次？"
          : voice.status === "failed"
            ? "语音输入失败，请重试。"
            : voice.status === "cancelled"
              ? "已取消"
              : voice.status === "submitted"
                ? "已发送"
                : "");

  return (
    <div className="microphone-control" data-status={voice.status}>
      <button
        className="microphone-button"
        type="button"
        disabled={disabled || processing}
        aria-label={recording ? "结束录音" : processing ? "正在识别语音" : "开始录音"}
        title={recording ? "结束录音" : "开始录音"}
        onClick={() => void run(recording ? "stop" : "start")}
      >
        {processing ? (
          <LoaderCircle className="microphone-button__spinner" size={18} aria-hidden="true" />
        ) : recording ? (
          <Square size={15} fill="currentColor" aria-hidden="true" />
        ) : (
          <Mic size={18} aria-hidden="true" />
        )}
      </button>
      <p className="microphone-status" role="status" aria-live="polite">
        {statusCopy}
      </p>
    </div>
  );
}
