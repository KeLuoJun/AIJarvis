import {
  Bot,
  Check,
  ExternalLink,
  Eye,
  Layers3,
  Mic2,
  Pause,
  Play,
  Power,
  RefreshCw,
  Sparkles,
  Trash2,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { PetSprite } from "../components/PetSprite";
import { MessageInput } from "../components/MessageInput";
import { MicrophoneButton } from "../components/MicrophoneButton";
import { SpeechBubble } from "../components/SpeechBubble";
import { useChatSession } from "../hooks/useChatSession";
import {
  closeAppWindow,
  describeIpcError,
  getBootstrapState,
  getRuntimeState,
  listenClearTransientOutput,
  listenRuntimeState,
  quitApp,
  setFeatureEnabled,
  setPrivacyPaused,
  showAppWindow,
} from "../lib/ipc";
import {
  appWindowKinds,
  type AppWindowKind,
  type BootstrapState,
  type ConfigDiagnostic,
  type ResourceAvailability,
  type RuntimeFeature,
  type RuntimeSnapshot,
} from "../lib/types";

type LoadState<T> =
  | { phase: "loading" }
  | { phase: "ready"; value: T }
  | { phase: "error"; message: string };

type RuntimeAction = "privacy" | RuntimeFeature;

const windowCopy: Record<AppWindowKind, { name: string; detail: string }> = {
  control: { name: "控制台", detail: "状态与窗口管理" },
  pet: { name: "桌宠", detail: "桌面陪伴窗口" },
  danmaku: { name: "弹幕层", detail: "游戏场景覆盖层" },
};

const resourceCopy: Array<{
  key: keyof ResourceAvailability;
  name: string;
}> = [
  { key: "modelConfig", name: "模型配置" },
  { key: "defaultsConfig", name: "运行参数" },
  { key: "petManifest", name: "桌宠资源" },
];

const featureCopy: Array<{
  feature: RuntimeFeature;
  key: keyof RuntimeSnapshot["features"];
  name: string;
  detail: string;
  icon: typeof Eye;
}> = [
  {
    feature: "screen",
    key: "screenEnabled",
    name: "屏幕感知",
    detail: "观察当前桌面场景",
    icon: Eye,
  },
  {
    feature: "systemAudio",
    key: "systemAudioEnabled",
    name: "系统声音",
    detail: "识别设备播放内容",
    icon: Mic2,
  },
  {
    feature: "proactiveResponse",
    key: "proactiveResponseEnabled",
    name: "主动响应",
    detail: "允许 Jarvis 主动发言",
    icon: Sparkles,
  },
];

const diagnosticSourceCopy: Record<ConfigDiagnostic["source"], string> = {
  startupState: "启动状态",
  settings: "功能设置",
};

const diagnosticKindCopy: Record<ConfigDiagnostic["kind"], string> = {
  missing: "缺失，已使用保守默认值",
  invalid: "格式无效，已使用保守默认值",
  readFailed: "读取失败，已使用保守默认值",
  writeFailed: "保存失败，请重试",
};

const runtimeActionCopy: Record<RuntimeAction, string> = {
  privacy: "隐私暂停",
  screen: "屏幕感知",
  systemAudio: "系统声音",
  proactiveResponse: "主动响应",
};

function SwitchControl({
  checked,
  disabled,
  label,
  onChange,
}: {
  checked: boolean;
  disabled: boolean;
  label: string;
  onChange: (checked: boolean) => void;
}) {
  return (
    <label className="switch-control">
      <span className="sr-only">{label}</span>
      <input
        type="checkbox"
        role="switch"
        checked={checked}
        disabled={disabled}
        onChange={(event) => onChange(event.currentTarget.checked)}
      />
      <span className="switch-control__track" aria-hidden="true">
        <span className="switch-control__thumb" />
      </span>
    </label>
  );
}

export function ControlWindow() {
  const [bootstrapState, setBootstrapState] = useState<LoadState<BootstrapState>>({
    phase: "loading",
  });
  const [runtimeState, setRuntimeState] = useState<LoadState<RuntimeSnapshot>>({
    phase: "loading",
  });
  const [pendingWindow, setPendingWindow] = useState<AppWindowKind>();
  const [pendingRuntime, setPendingRuntime] = useState<RuntimeAction>();
  const [runtimeActionError, setRuntimeActionError] = useState<string>();
  const [shellActionError, setShellActionError] = useState<string>();
  const [transientClearRevision, setTransientClearRevision] = useState(0);
  const [quitting, setQuitting] = useState(false);
  const runtimeOperationLocked = useRef(false);
  const runtimeRevision = useRef(-1);

  const applyRuntimeSnapshot = useCallback((snapshot: RuntimeSnapshot) => {
    if (snapshot.revision <= runtimeRevision.current) return;
    runtimeRevision.current = snapshot.revision;
    setRuntimeActionError(undefined);
    setRuntimeState({ phase: "ready", value: snapshot });
  }, []);

  const refreshBootstrap = useCallback(async () => {
    setBootstrapState((current) =>
      current.phase === "ready" ? current : { phase: "loading" },
    );
    try {
      setBootstrapState({ phase: "ready", value: await getBootstrapState() });
    } catch (error) {
      setBootstrapState({ phase: "error", message: describeIpcError(error) });
    }
  }, []);

  const refreshRuntime = useCallback(async () => {
    setRuntimeState((current) =>
      current.phase === "ready" ? current : { phase: "loading" },
    );
    try {
      applyRuntimeSnapshot(await getRuntimeState());
    } catch (error) {
      setRuntimeState((current) =>
        current.phase === "ready"
          ? current
          : { phase: "error", message: describeIpcError(error) },
      );
    }
  }, [applyRuntimeSnapshot]);

  const refresh = useCallback(async () => {
    setRuntimeActionError(undefined);
    setShellActionError(undefined);
    await Promise.all([refreshBootstrap(), refreshRuntime()]);
  }, [refreshBootstrap, refreshRuntime]);

  useEffect(() => {
    void refreshBootstrap();
  }, [refreshBootstrap]);

  useEffect(() => {
    let active = true;
    const unlisteners: Array<() => void> = [];

    const connect = async () => {
      try {
        const unlistenRuntime = await listenRuntimeState((snapshot) => {
          if (active) applyRuntimeSnapshot(snapshot);
        });
        if (!active) {
          unlistenRuntime();
          return;
        }
        unlisteners.push(unlistenRuntime);

        const unlistenClear = await listenClearTransientOutput((event) => {
          if (active) {
            setTransientClearRevision((current) =>
              Math.max(current, event.revision),
            );
          }
        });
        if (!active) {
          unlistenClear();
          return;
        }
        unlisteners.push(unlistenClear);
        await refreshRuntime();
      } catch (error) {
        if (active) {
          setRuntimeState((current) =>
            current.phase === "ready"
              ? current
              : {
                  phase: "error",
                  message: describeIpcError(error),
                },
          );
        }
      }
    };

    void connect();
    return () => {
      active = false;
      for (const unlisten of unlisteners) unlisten();
    };
  }, [applyRuntimeSnapshot, refreshRuntime]);

  const readyCount = useMemo(() => {
    if (bootstrapState.phase !== "ready") return 0;
    return resourceCopy.filter(
      ({ key }) => bootstrapState.value.resourceStatus[key],
    ).length;
  }, [bootstrapState]);

  const runtime = runtimeState.phase === "ready" ? runtimeState.value : undefined;
  const chat = useChatSession(runtime?.runtimeGeneration, runtime?.revision);
  const controlsDisabled =
    !runtime || runtime.lifecycleState !== "running" || pendingRuntime !== undefined;
  const privacyPaused = runtime?.privacyState === "paused";
  const privacyActive = runtime?.privacyState === "active";
  const lifecycleCopy =
    runtime?.lifecycleState === "running"
      ? "运行中"
      : runtime?.lifecycleState === "shuttingDown"
        ? "正在停止"
        : runtime?.lifecycleState === "stopped"
          ? "已停止"
          : "连接中";

  const changeWindow = async (kind: AppWindowKind, open: boolean) => {
    setPendingWindow(kind);
    setShellActionError(undefined);
    try {
      if (open) await showAppWindow(kind);
      else await closeAppWindow(kind);
      await refreshBootstrap();
    } catch (error) {
      setShellActionError(describeIpcError(error));
    } finally {
      setPendingWindow(undefined);
    }
  };

  const runRuntimeAction = async (
    action: RuntimeAction,
    request: () => Promise<RuntimeSnapshot>,
  ) => {
    if (runtimeOperationLocked.current) return;
    runtimeOperationLocked.current = true;
    setPendingRuntime(action);
    setRuntimeActionError(undefined);
    try {
      applyRuntimeSnapshot(await request());
    } catch (error) {
      setRuntimeActionError(describeIpcError(error));
    } finally {
      runtimeOperationLocked.current = false;
      setPendingRuntime(undefined);
    }
  };

  const requestQuit = async () => {
    setQuitting(true);
    setShellActionError(undefined);
    try {
      await quitApp();
    } catch (error) {
      setQuitting(false);
      setShellActionError(describeIpcError(error));
    }
  };

  const loadError =
    runtimeState.phase === "error"
      ? runtimeState.message
      : bootstrapState.phase === "error"
        ? bootstrapState.message
        : undefined;
  const visibleError = runtimeActionError ?? shellActionError ?? loadError;
  const runtimeStatusCopy = pendingRuntime
    ? `正在更新${runtimeActionCopy[pendingRuntime]}。`
    : runtimeState.phase === "error"
      ? undefined
      : runtimeState.phase === "loading"
        ? "正在连接本地运行时。"
        : privacyPaused
          ? "隐私暂停已开启，采集、推理与临时输出均已停止。"
          : "隐私暂停已关闭，本地运行时正在工作。";

  return (
    <main
      className="control-window"
      data-window="control"
      data-transient-clear-revision={transientClearRevision}
    >
      <aside className="signal-rail" aria-label="本地运行信号">
        <div className="signal-rail__monogram" aria-hidden="true">J</div>
        <div className="signal-rail__line" aria-hidden="true">
          <span className={`signal-node ${privacyActive ? "signal-node--active" : ""}`} />
          <span className="signal-node" />
          <span className={`signal-node ${privacyPaused ? "signal-node--coral" : ""}`} />
        </div>
        <span className="signal-rail__local">LOCAL</span>
      </aside>

      <section className="control-surface">
        {runtimeStatusCopy ? (
          <p className="sr-only" role="status" aria-live="polite" aria-atomic="true">
            {runtimeStatusCopy}
          </p>
        ) : null}
        <header className="titlebar" data-tauri-drag-region>
          <div className="titlebar__identity" data-tauri-drag-region>
            <span className="eyebrow">DESKTOP COMPANION</span>
            <h1>AI Jarvis</h1>
          </div>
          <div className="titlebar__actions">
            <button className="icon-button" type="button" onClick={() => void refresh()} title="刷新状态" aria-label="刷新状态" disabled={bootstrapState.phase === "loading" || runtimeState.phase === "loading"}>
              <RefreshCw size={17} aria-hidden="true" />
            </button>
            <button className="icon-button" type="button" onClick={() => void changeWindow("control", false)} title="隐藏控制台" aria-label="隐藏控制台">
              <X size={18} aria-hidden="true" />
            </button>
          </div>
        </header>

        {visibleError ? (
          <section className="error-band" role="alert">
            <div>
              <span className="eyebrow">LOCAL RUNTIME</span>
              <h2>本地运行时不可用</h2>
              <p>{visibleError}</p>
            </div>
            <button className="command-button" type="button" onClick={() => void refresh()}>
              <RefreshCw size={16} aria-hidden="true" />重试
            </button>
          </section>
        ) : null}

        <section className={`status-band ${privacyPaused ? "status-band--paused" : ""}`} aria-busy={runtimeState.phase === "loading" || pendingRuntime !== undefined}>
          <div className="status-band__copy">
            <span className="eyebrow">PRIVACY CORE</span>
            <div className="status-band__headline">
              <strong>{runtimeState.phase === "ready" ? privacyPaused ? "已暂停" : "本地运行" : "连接中"}</strong>
            </div>
            <p className="version-line">
              {privacyPaused ? "采集、推理与临时输出均已停止" : `运行时 ${lifecycleCopy} · 本地资源 ${readyCount}/3`}
            </p>
          </div>
          <div className="privacy-command">
            <div>
              {privacyPaused ? <Pause size={18} aria-hidden="true" /> : <Play size={18} aria-hidden="true" />}
              <span>{privacyPaused ? "隐私暂停" : "隐私保护"}</span>
            </div>
            <SwitchControl label="隐私暂停" checked={privacyPaused} disabled={controlsDisabled} onChange={(checked) => void runRuntimeAction("privacy", () => setPrivacyPaused(checked))} />
          </div>
          <PetSprite className="control-pet" state={privacyActive ? "idle" : "paused"} />
        </section>

        <section className="chat-workspace" aria-labelledby="chat-title">
          <div className="chat-workspace__header">
            <div>
              <span className="eyebrow">DIRECT CHAT</span>
              <h2 id="chat-title">当前会话</h2>
            </div>
            <button
              className="icon-button"
              type="button"
              title="清空当前会话"
              aria-label="清空当前会话"
              disabled={chat.exchanges.length === 0}
              onClick={() => void chat.clear()}
            >
              <Trash2 size={16} aria-hidden="true" />
            </button>
          </div>
          <div className="chat-workspace__body">
            <div className="chat-transcript" aria-label="当前对话">
              {chat.exchanges.length > 0 ? (
                chat.exchanges.slice(-6).map((exchange) => (
                  <SpeechBubble key={exchange.key} exchange={exchange} />
                ))
              ) : (
                <p className="chat-empty">会话尚未开始。</p>
              )}
            </div>
            <div className="chat-composer">
              <div className="chat-composer__controls">
                <MessageInput
                  busy={chat.submitting}
                  disabled={!chat.ready || !privacyActive || runtime?.lifecycleState !== "running"}
                  onSend={chat.send}
                />
                <MicrophoneButton
                  disabled={!privacyActive || runtime?.lifecycleState !== "running"}
                />
              </div>
              {chat.error ? <p className="chat-error" role="alert">{chat.error}</p> : null}
            </div>
          </div>
        </section>

        <div className="control-columns">
          <section className="flat-section" aria-labelledby="feature-title">
            <div className="section-heading">
              <div><span className="eyebrow">SENSORS</span><h2 id="feature-title">感知与响应</h2></div>
              <Eye size={19} aria-hidden="true" />
            </div>
            <div className="row-list">
              {featureCopy.map(({ feature, key, name, detail, icon: Icon }) => (
                <div className="setting-row" key={feature}>
                  <Icon size={16} aria-hidden="true" />
                  <div><strong>{name}</strong><span>{detail}</span></div>
                  <SwitchControl label={name} checked={runtime?.features[key] ?? false} disabled={controlsDisabled} onChange={(checked) => void runRuntimeAction(feature, () => setFeatureEnabled(feature, checked))} />
                </div>
              ))}
            </div>
            {runtime?.diagnostics.length ? (
              <div className="diagnostic-list" aria-label="配置诊断">
                {runtime.diagnostics.map((diagnostic, index) => (
                  <p key={`${diagnostic.source}-${diagnostic.kind}-${index}`}>
                    <strong>{diagnosticSourceCopy[diagnostic.source]}</strong>
                    <span>{diagnosticKindCopy[diagnostic.kind]}</span>
                  </p>
                ))}
              </div>
            ) : <p className="diagnostic-ok">配置状态正常</p>}
          </section>

          <section className="flat-section" aria-labelledby="resource-title">
            <div className="section-heading">
              <div><span className="eyebrow">RESOURCE CHECK</span><h2 id="resource-title">本地资源</h2></div>
              <Bot size={19} aria-hidden="true" />
            </div>
            <div className="row-list">
              {resourceCopy.map(({ key, name }) => {
                const available = bootstrapState.phase === "ready" && bootstrapState.value.resourceStatus[key];
                return (
                  <div className="status-row" key={key}>
                    <span>{name}</span>
                    <span className={`status-chip ${available ? "status-chip--ok" : "status-chip--pending"}`}>
                      {available ? <Check size={13} aria-hidden="true" /> : null}
                      {bootstrapState.phase === "loading" ? "检查中" : available ? "可用" : "缺失"}
                    </span>
                  </div>
                );
              })}
            </div>
          </section>

          <section className="flat-section" aria-labelledby="window-title">
            <div className="section-heading">
              <div><span className="eyebrow">WINDOW ROUTING</span><h2 id="window-title">窗口</h2></div>
              <Layers3 size={19} aria-hidden="true" />
            </div>
            <div className="row-list">
              {appWindowKinds.map((kind) => {
                const state = bootstrapState.phase === "ready" ? bootstrapState.value.windows.find((window) => window.kind === kind) : undefined;
                const isCurrent = bootstrapState.phase === "ready" && bootstrapState.value.currentWindow === kind;
                const open = state?.open ?? false;
                return (
                  <div className="window-row" key={kind}>
                    <div><strong>{windowCopy[kind].name}</strong><span>{windowCopy[kind].detail}</span></div>
                    {isCurrent ? <span className="current-label">当前</span> : (
                      <button className="window-command" type="button" disabled={bootstrapState.phase !== "ready" || pendingWindow !== undefined} onClick={() => void changeWindow(kind, !open)}>
                        {open ? <X size={15} aria-hidden="true" /> : <ExternalLink size={15} aria-hidden="true" />}
                        {pendingWindow === kind ? "处理中" : open ? "关闭" : "打开"}
                      </button>
                    )}
                  </div>
                );
              })}
            </div>
          </section>
        </div>

        <footer className="control-footer">
          <span>本地运行 · {lifecycleCopy} · 状态修订 {runtime?.revision ?? "--"}</span>
          <button className="quit-button" type="button" disabled={quitting} onClick={() => void requestQuit()}>
            <Power size={16} aria-hidden="true" />{quitting ? "正在退出" : "退出 AI Jarvis"}
          </button>
        </footer>
      </section>
    </main>
  );
}
