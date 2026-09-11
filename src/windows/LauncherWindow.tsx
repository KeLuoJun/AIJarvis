import { Check, CircleAlert, Download, LoaderCircle, Sparkles } from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import { PetSprite } from "../components/PetSprite";
import {
  listenBootstrapProgress,
  summonAiJarvis,
} from "../lib/ipc";
import type { BootstrapProgress } from "../lib/types";

type LauncherState =
  | { phase: "idle" }
  | { phase: "running"; progress: BootstrapProgress }
  | { phase: "failed"; progress: BootstrapProgress };

const steps = [
  { id: "resources", label: "应用资源" },
  { id: "runtime", label: "本地运行环境" },
  { id: "models", label: "模型文件" },
  { id: "modelService", label: "模型服务" },
] as const;

function formatBytes(value: number): string {
  if (value <= 0) return "0 MB";
  const units = ["B", "KB", "MB", "GB"];
  const unit = Math.min(Math.floor(Math.log(value) / Math.log(1024)), units.length - 1);
  const amount = value / 1024 ** unit;
  return `${amount.toFixed(unit >= 3 ? 1 : 0)} ${units[unit]}`;
}

function stepRank(step: string): number {
  if (step === "resources") return 0;
  if (step === "runtime") return 1;
  if (["qwenModel", "qwenVision", "senseVoiceModel", "senseVoiceTokens"].includes(step)) return 2;
  if (step === "modelService") return 3;
  if (step === "complete") return 4;
  return -1;
}

export function LauncherWindow() {
  const [state, setState] = useState<LauncherState>({ phase: "idle" });

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listenBootstrapProgress((progress) => {
      if (disposed) return;
      setState({
        phase: progress.phase === "failed" ? "failed" : "running",
        progress,
      });
    }).then((dispose) => {
      if (disposed) dispose();
      else unlisten = dispose;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const progress = state.phase === "idle" ? null : state.progress;
  const activeRank = progress ? stepRank(progress.step) : -1;
  const percent = useMemo(() => {
    if (!progress?.totalBytes) return 0;
    return Math.min(100, Math.round((progress.completedBytes / progress.totalBytes) * 100));
  }, [progress]);
  const busy = state.phase === "running";

  async function summon() {
    setState({
      phase: "running",
      progress: {
        phase: "checking",
        step: "resources",
        label: "应用资源",
        detail: "准备启动检查",
        artifactIndex: 0,
        artifactCount: 0,
        currentBytes: 0,
        currentTotalBytes: 0,
        completedBytes: 0,
        totalBytes: 0,
      },
    });
    try {
      await summonAiJarvis();
    } catch {
      setState((current) => {
        if (current.phase === "failed") return current;
        return {
          phase: "failed",
          progress: {
            ...(current.phase === "idle" ? {
              phase: "failed" as const,
              step: "failed",
              label: "启动未完成",
              detail: "本地启动服务没有返回可用信息，请重试。",
              artifactIndex: 0,
              artifactCount: 0,
              currentBytes: 0,
              currentTotalBytes: 0,
              completedBytes: 0,
              totalBytes: 0,
            } : current.progress),
            phase: "failed",
          },
        };
      });
    }
  }

  return (
    <main className="launcher-window" data-window="launcher" data-phase={state.phase}>
      <section className="launcher-character" aria-hidden="true" data-tauri-drag-region>
        <div className="launcher-signal">
          <span />
          <span />
          <span />
        </div>
        <PetSprite className="launcher-pet" state={busy ? "normal" : "idle"} animated />
        <div className="launcher-ground" />
      </section>

      <section className="launcher-console">
        <header className="launcher-heading">
          <h1>AI Jarvis</h1>
          <p>{state.phase === "idle" ? "准备好后，唤醒你的本地桌面伙伴。" : progress?.detail}</p>
        </header>

        {state.phase === "idle" ? (
          <div className="launcher-action">
            <button className="summon-button" type="button" onClick={() => void summon()}>
              <Sparkles size={18} />
              召唤 AI Jarvis
            </button>
            <span className="launcher-local-badge">模型运行在本地</span>
          </div>
        ) : (
          <div className="bootstrap-panel" aria-live="polite">
            <div className="bootstrap-steps">
              {steps.map((step, index) => {
                const complete = activeRank > index || progress?.phase === "complete";
                const active = activeRank === index && state.phase !== "failed";
                return (
                  <div className="bootstrap-step" data-state={complete ? "complete" : active ? "active" : "pending"} key={step.id}>
                    <span className="bootstrap-step__icon">
                      {complete ? <Check size={14} /> : active ? <LoaderCircle size={14} /> : <span />}
                    </span>
                    <span>{step.label}</span>
                  </div>
                );
              })}
            </div>

            {progress?.totalBytes ? (
              <div className="download-progress">
                <div className="download-progress__copy">
                  <span><Download size={14} /> {progress.label}</span>
                  <strong>{percent}%</strong>
                </div>
                <div className="download-progress__track" role="progressbar" aria-label="模型下载总进度" aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent}>
                  <span style={{ width: `${percent}%` }} />
                </div>
                <div className="download-progress__meta">
                  <span>{formatBytes(progress.completedBytes)} / {formatBytes(progress.totalBytes)}</span>
                  {progress.artifactCount > 0 && <span>{progress.artifactIndex} / {progress.artifactCount}</span>}
                </div>
              </div>
            ) : (
              <div className="bootstrap-activity">
                {state.phase === "failed" ? <CircleAlert size={17} /> : <LoaderCircle size={17} />}
                <span>{progress?.label}</span>
              </div>
            )}

            {state.phase === "failed" && (
              <div className="bootstrap-failure" role="alert">
                <p>{progress?.detail}</p>
                <button type="button" onClick={() => void summon()}>重新检查</button>
              </div>
            )}
          </div>
        )}
      </section>
    </main>
  );
}
