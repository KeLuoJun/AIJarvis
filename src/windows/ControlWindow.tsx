import {
  Bot,
  Check,
  ExternalLink,
  Layers3,
  Power,
  RefreshCw,
  X,
} from "lucide-react";
import { useCallback, useEffect, useMemo, useState } from "react";

import { PetSprite } from "../components/PetSprite";
import {
  closeAppWindow,
  describeIpcError,
  getBootstrapState,
  quitApp,
  showAppWindow,
} from "../lib/ipc";
import {
  appWindowKinds,
  type AppWindowKind,
  type BootstrapState,
  type ResourceAvailability,
} from "../lib/types";

type LoadState =
  | { phase: "loading" }
  | { phase: "ready"; value: BootstrapState }
  | { phase: "error"; message: string };

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

export function ControlWindow() {
  const [loadState, setLoadState] = useState<LoadState>({ phase: "loading" });
  const [pendingWindow, setPendingWindow] = useState<AppWindowKind>();
  const [quitting, setQuitting] = useState(false);

  const refresh = useCallback(async () => {
    setLoadState((current) =>
      current.phase === "ready" ? current : { phase: "loading" },
    );
    try {
      setLoadState({ phase: "ready", value: await getBootstrapState() });
    } catch (error) {
      setLoadState({ phase: "error", message: describeIpcError(error) });
    }
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const readyCount = useMemo(() => {
    if (loadState.phase !== "ready") return 0;
    return resourceCopy.filter(({ key }) => loadState.value.resourceStatus[key]).length;
  }, [loadState]);

  const changeWindow = async (kind: AppWindowKind, open: boolean) => {
    setPendingWindow(kind);
    try {
      if (open) await showAppWindow(kind);
      else await closeAppWindow(kind);
      await refresh();
    } catch (error) {
      setLoadState({ phase: "error", message: describeIpcError(error) });
    } finally {
      setPendingWindow(undefined);
    }
  };

  const requestQuit = async () => {
    setQuitting(true);
    try {
      await quitApp();
    } catch (error) {
      setQuitting(false);
      setLoadState({ phase: "error", message: describeIpcError(error) });
    }
  };

  return (
    <main className="control-window" data-window="control">
      <aside className="signal-rail" aria-label="本地运行信号">
        <div className="signal-rail__monogram" aria-hidden="true">
          J
        </div>
        <div className="signal-rail__line" aria-hidden="true">
          <span className="signal-node signal-node--active" />
          <span className="signal-node" />
          <span className="signal-node signal-node--coral" />
        </div>
        <span className="signal-rail__local">LOCAL</span>
      </aside>

      <section className="control-surface">
        <header className="titlebar" data-tauri-drag-region>
          <div className="titlebar__identity" data-tauri-drag-region>
            <span className="eyebrow">DESKTOP COMPANION</span>
            <h1>AI Jarvis</h1>
          </div>
          <div className="titlebar__actions">
            <button
              className="icon-button"
              type="button"
              onClick={() => void refresh()}
              title="刷新状态"
              aria-label="刷新状态"
              disabled={loadState.phase === "loading"}
            >
              <RefreshCw size={17} aria-hidden="true" />
            </button>
            <button
              className="icon-button"
              type="button"
              onClick={() => void changeWindow("control", false)}
              title="隐藏控制台"
              aria-label="隐藏控制台"
            >
              <X size={18} aria-hidden="true" />
            </button>
          </div>
        </header>

        {loadState.phase === "error" ? (
          <section className="error-band" role="alert">
            <div>
              <span className="eyebrow">CONNECTION</span>
              <h2>无法读取本地运行状态</h2>
              <p>{loadState.message}</p>
            </div>
            <button className="command-button" type="button" onClick={() => void refresh()}>
              <RefreshCw size={16} aria-hidden="true" />
              重试
            </button>
          </section>
        ) : null}

        <section className="status-band" aria-busy={loadState.phase === "loading"}>
          <div className="status-band__copy">
            <span className="eyebrow">SYSTEM READINESS</span>
            <div className="status-band__headline">
              <strong>{loadState.phase === "ready" ? `${readyCount}/3` : "--"}</strong>
              <span>{readyCount === 3 ? "本地资源就绪" : "正在核对本地资源"}</span>
            </div>
            <p className="version-line">
              {loadState.phase === "ready"
                ? `应用版本 ${loadState.value.appVersion}`
                : "正在连接 Rust 运行时"}
            </p>
          </div>
          <PetSprite className="control-pet" state="idle" />
        </section>

        <div className="control-columns">
          <section className="flat-section" aria-labelledby="resource-title">
            <div className="section-heading">
              <div>
                <span className="eyebrow">RESOURCE CHECK</span>
                <h2 id="resource-title">本地资源</h2>
              </div>
              <Bot size={19} aria-hidden="true" />
            </div>
            <div className="row-list">
              {resourceCopy.map(({ key, name }) => {
                const available =
                  loadState.phase === "ready" && loadState.value.resourceStatus[key];
                return (
                  <div className="status-row" key={key}>
                    <span>{name}</span>
                    <span
                      className={`status-chip ${available ? "status-chip--ok" : "status-chip--pending"}`}
                    >
                      {available ? <Check size={13} aria-hidden="true" /> : null}
                      {loadState.phase === "loading"
                        ? "检查中"
                        : available
                          ? "可用"
                          : "缺失"}
                    </span>
                  </div>
                );
              })}
            </div>
          </section>

          <section className="flat-section" aria-labelledby="window-title">
            <div className="section-heading">
              <div>
                <span className="eyebrow">WINDOW ROUTING</span>
                <h2 id="window-title">窗口</h2>
              </div>
              <Layers3 size={19} aria-hidden="true" />
            </div>
            <div className="row-list">
              {appWindowKinds.map((kind) => {
                const state =
                  loadState.phase === "ready"
                    ? loadState.value.windows.find((window) => window.kind === kind)
                    : undefined;
                const isCurrent =
                  loadState.phase === "ready" && loadState.value.currentWindow === kind;
                const open = state?.open ?? false;
                return (
                  <div className="window-row" key={kind}>
                    <div>
                      <strong>{windowCopy[kind].name}</strong>
                      <span>{windowCopy[kind].detail}</span>
                    </div>
                    {isCurrent ? (
                      <span className="current-label">当前</span>
                    ) : (
                      <button
                        className="window-command"
                        type="button"
                        disabled={loadState.phase !== "ready" || pendingWindow === kind}
                        onClick={() => void changeWindow(kind, !open)}
                      >
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
          <span>本地运行 · 回环通信</span>
          <button
            className="quit-button"
            type="button"
            disabled={quitting}
            onClick={() => void requestQuit()}
          >
            <Power size={16} aria-hidden="true" />
            {quitting ? "正在退出" : "退出 AI Jarvis"}
          </button>
        </footer>
      </section>
    </main>
  );
}
