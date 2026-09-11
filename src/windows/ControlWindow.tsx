import {
  Check,
  CalendarDays,
  Brain,
  ChevronDown,
  Pause,
  Pencil,
  Play,
  Plus,
  Power,
  Trash2,
  X,
} from "lucide-react";
import { useCallback, useEffect, useRef, useState, type FormEvent } from "react";

import { MessageInput } from "../components/MessageInput";
import { MicrophoneButton } from "../components/MicrophoneButton";
import { PetSprite } from "../components/PetSprite";
import { SpeechBubble } from "../components/SpeechBubble";
import { useChatSession } from "../hooks/useChatSession";
import {
  closeAppWindow,
  createPersona,
  deletePersona,
  deleteDailySummary,
  describeIpcError,
  getPersonas,
  getDailySummaries,
  getDailySummary,
  generateDailySummary,
  getRuntimeState,
  listenRuntimeState,
  quitApp,
  selectPersona,
  setFeatureEnabled,
  getMemories,
  updateMemory,
  deleteMemory,
  clearMemories,
  setMemoryEnabled,
  setPrivacyPaused,
  updatePersona,
} from "../lib/ipc";
import type { DailySummary, DailySummaryListItem, MemoryItem, MemoryKind, PersonaProfile, PersonaState, RuntimeSnapshot } from "../lib/types";

type PersonaDraft = Pick<PersonaProfile, "name" | "prompt"> & { id?: string };

const emptyDraft: PersonaDraft = { name: "", prompt: "" };

export function ControlWindow() {
  const [runtime, setRuntime] = useState<RuntimeSnapshot>();
  const [personas, setPersonas] = useState<PersonaState>();
  const [draft, setDraft] = useState<PersonaDraft>();
  const [error, setError] = useState<string>();
  const [runtimePending, setRuntimePending] = useState(false);
  const [personaPending, setPersonaPending] = useState(false);
  const [quitting, setQuitting] = useState(false);
  const [scheduleOpen, setScheduleOpen] = useState(false);
  const [scheduleItems, setScheduleItems] = useState<DailySummaryListItem[]>([]);
  const [schedule, setSchedule] = useState<DailySummary | null>(null);
  const [scheduleBusy, setScheduleBusy] = useState(false);
  const [memoryOpen, setMemoryOpen] = useState(false);
  const [memories, setMemories] = useState<MemoryItem[]>([]);
  const [memoryEnabled, setMemoryEnabledState] = useState(true);
  const [memoryBusy, setMemoryBusy] = useState(false);
  const [memoryDraft, setMemoryDraft] = useState<MemoryItem>();
  const runtimeRevision = useRef(-1);
  const conversationScroll = useRef<HTMLDivElement>(null);
  const chat = useChatSession(runtime?.runtimeGeneration, runtime?.revision, {
    includeProactive: false,
  });

  const applyRuntime = useCallback((snapshot: RuntimeSnapshot) => {
    if (snapshot.revision <= runtimeRevision.current) return;
    runtimeRevision.current = snapshot.revision;
    setRuntime(snapshot);
    setError(undefined);
  }, []);

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;
    const connect = async () => {
      try {
        const [snapshot, personaState, stopListening] = await Promise.all([
          getRuntimeState(),
          getPersonas(),
          listenRuntimeState((next) => {
            if (active) applyRuntime(next);
          }),
        ]);
        if (!active) {
          stopListening();
          return;
        }
        unlisten = stopListening;
        applyRuntime(snapshot);
        setPersonas(personaState);
      } catch (reason) {
        if (active) setError(describeIpcError(reason));
      }
    };
    void connect();
    return () => {
      active = false;
      unlisten?.();
    };
  }, [applyRuntime]);

  useEffect(() => {
    if (!scheduleOpen) return;
    void getDailySummaries().then((items) => {
      setScheduleItems(items);
      if (items[0]) void getDailySummary(items[0].date).then(setSchedule);
    }).catch((reason) => setError(describeIpcError(reason)));
  }, [scheduleOpen]);

  useEffect(() => {
    if (!memoryOpen) return;
    void getMemories().then(([enabled, items]) => {
      setMemoryEnabledState(enabled);
      setMemories(items);
    }).catch((reason) => setError(describeIpcError(reason)));
  }, [memoryOpen]);

  const toggleMemory = async () => {
    setMemoryBusy(true);
    try {
      const [enabled, items] = await setMemoryEnabled(!memoryEnabled);
      setMemoryEnabledState(enabled); setMemories(items);
    } catch (reason) { setError(describeIpcError(reason)); }
    finally { setMemoryBusy(false); }
  };

  const saveMemory = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (!memoryDraft || memoryBusy || !memoryDraft.text.trim()) return;
    setMemoryBusy(true);
    try {
      const item = await updateMemory(memoryDraft.id, memoryDraft.kind, memoryDraft.text);
      setMemories((current) => current.map((entry) => entry.id === item.id ? item : entry));
      setMemoryDraft(undefined);
    } catch (reason) { setError(describeIpcError(reason)); }
    finally { setMemoryBusy(false); }
  };

  const removeMemory = async (id: number) => {
    if (memoryBusy) return;
    setMemoryBusy(true);
    try { await deleteMemory(id); setMemories((current) => current.filter((item) => item.id !== id)); }
    catch (reason) { setError(describeIpcError(reason)); }
    finally { setMemoryBusy(false); }
  };

  const openTodaySchedule = async () => {
    const today = new Date().toISOString().slice(0, 10);
    setScheduleBusy(true);
    setError(undefined);
    try {
      const result = await generateDailySummary(today);
      setSchedule(result);
      setScheduleItems(await getDailySummaries());
    } catch (reason) {
      setError(describeIpcError(reason));
    } finally {
      setScheduleBusy(false);
    }
  };

  const removeSchedule = async () => {
    if (!schedule || scheduleBusy) return;
    setScheduleBusy(true);
    try {
      await deleteDailySummary(schedule.date);
      const items = await getDailySummaries();
      setScheduleItems(items);
      setSchedule(items[0] ? await getDailySummary(items[0].date) : null);
    } catch (reason) {
      setError(describeIpcError(reason));
    } finally {
      setScheduleBusy(false);
    }
  };

  useEffect(() => {
    const viewport = conversationScroll.current;
    if (viewport) viewport.scrollTop = viewport.scrollHeight;
  }, [chat.exchanges]);

  useEffect(() => {
    if (!draft) return;
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !personaPending) setDraft(undefined);
    };
    window.addEventListener("keydown", closeOnEscape);
    return () => window.removeEventListener("keydown", closeOnEscape);
  }, [draft, personaPending]);

  const activePersona = personas?.personas.find(
    (persona) => persona.id === personas.activeId,
  );
  const privacyActive = runtime?.privacyState === "active";
  const active = Boolean(
    privacyActive &&
    runtime?.features.screenEnabled &&
    runtime.features.systemAudioEnabled &&
    runtime.features.proactiveResponseEnabled,
  );
  const available = runtime?.lifecycleState === "running";

  const toggleRuntime = async () => {
    if (!runtime || runtimePending) return;
    setRuntimePending(true);
    setError(undefined);
    try {
      if (active) {
        applyRuntime(await setPrivacyPaused(true));
      } else {
        await setFeatureEnabled("screen", true);
        await setFeatureEnabled("systemAudio", true);
        await setFeatureEnabled("proactiveResponse", true);
        applyRuntime(await setPrivacyPaused(false));
      }
    } catch (reason) {
      setError(describeIpcError(reason));
    } finally {
      setRuntimePending(false);
    }
  };

  const choosePersona = async (id: string) => {
    if (!personas || id === personas.activeId || personaPending) return;
    setPersonaPending(true);
    setError(undefined);
    try {
      setPersonas(await selectPersona(id));
      await chat.clear();
    } catch (reason) {
      setError(describeIpcError(reason));
    } finally {
      setPersonaPending(false);
    }
  };

  const savePersona = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (!draft || personaPending) return;
    setPersonaPending(true);
    setError(undefined);
    try {
      const next = draft.id
        ? await updatePersona(draft.id, draft.name, draft.prompt)
        : await createPersona(draft.name, draft.prompt);
      setPersonas(next);
      setDraft(undefined);
      await chat.clear();
    } catch (reason) {
      setError(describeIpcError(reason));
    } finally {
      setPersonaPending(false);
    }
  };

  const removePersona = async () => {
    if (!activePersona || !personas || personas.personas.length === 1 || personaPending) return;
    setPersonaPending(true);
    setError(undefined);
    try {
      setPersonas(await deletePersona(activePersona.id));
      await chat.clear();
    } catch (reason) {
      setError(describeIpcError(reason));
    } finally {
      setPersonaPending(false);
    }
  };

  const requestQuit = async () => {
    setQuitting(true);
    setError(undefined);
    try {
      await quitApp();
    } catch (reason) {
      setQuitting(false);
      setError(describeIpcError(reason));
    }
  };

  return (
    <main className="product-shell" data-window="control">
      <header className="product-titlebar" data-tauri-drag-region>
        <div className="product-brand" data-tauri-drag-region>
          <span className="product-brand__mark" aria-hidden="true">J</span>
          <div data-tauri-drag-region>
            <strong>AI Jarvis</strong>
            <span>{activePersona?.name ?? "正在载入伙伴"}</span>
          </div>
        </div>
        <div className="product-titlebar__actions">
          <span className={`runtime-badge ${active ? "runtime-badge--active" : ""}`}>
            <span aria-hidden="true" />{active ? "正在陪伴" : "已暂停"}
          </span>
          <button className="icon-button product-icon-button" type="button" title="打开日程总结" aria-label="打开日程总结" onClick={() => setScheduleOpen(true)}>
            <CalendarDays size={17} aria-hidden="true" />
          </button>
          <button className="icon-button product-icon-button" type="button" title="隐藏窗口" aria-label="隐藏窗口" onClick={() => void closeAppWindow("control")}>
            <X size={18} aria-hidden="true" />
          </button>
        </div>
      </header>

      {error || chat.error ? <p className="product-alert" role="alert">{error ?? chat.error}</p> : null}

      <div className="product-body">
        <aside className="companion-panel" aria-label="当前伙伴">
          <div className="persona-voiceprint" aria-hidden="true">
            <span /><span /><span /><span /><span /><span /><span />
          </div>
          <PetSprite className="companion-panel__pet" state={active ? "idle" : "paused"} animated={active} />
          <div className="companion-panel__identity">
            <span>当前伙伴</span>
            <h1>{activePersona?.name ?? "加载中"}</h1>
          </div>

          <label className="persona-select">
            <span className="sr-only">选择人设</span>
            <select value={personas?.activeId ?? ""} disabled={!personas || personaPending} onChange={(event) => void choosePersona(event.currentTarget.value)}>
              {personas?.personas.map((persona) => <option key={persona.id} value={persona.id}>{persona.name}</option>)}
            </select>
            <ChevronDown size={16} aria-hidden="true" />
          </label>

          <div className="persona-actions">
            <button type="button" title="新建人设" aria-label="新建人设" onClick={() => setDraft(emptyDraft)}><Plus size={17} aria-hidden="true" /></button>
            <button type="button" title="编辑当前人设" aria-label="编辑当前人设" disabled={!activePersona} onClick={() => activePersona && setDraft(activePersona)}><Pencil size={16} aria-hidden="true" /></button>
            <button type="button" title="删除当前人设" aria-label="删除当前人设" disabled={!personas || personas.personas.length === 1 || personaPending} onClick={() => void removePersona()}><Trash2 size={16} aria-hidden="true" /></button>
          </div>

          <button className={`presence-toggle ${active ? "presence-toggle--active" : ""}`} type="button" disabled={!available || runtimePending} onClick={() => void toggleRuntime()}>
            {active ? <Pause size={17} aria-hidden="true" /> : <Play size={17} aria-hidden="true" />}
            <span>{runtimePending ? "正在切换" : active ? "暂停陪伴" : "继续陪伴"}</span>
          </button>

          <button className="window-command" type="button" onClick={() => setMemoryOpen(true)}>
            <Brain size={15} aria-hidden="true" />长期记忆
          </button>

          <button className="product-quit" type="button" title="退出应用" aria-label="退出应用" disabled={quitting} onClick={() => void requestQuit()}>
            <Power size={15} aria-hidden="true" />{quitting ? "正在退出" : "退出应用"}
          </button>
        </aside>

        <section className="conversation-panel" aria-labelledby="conversation-title">
          <header className="conversation-panel__header">
            <div><span>主动会话</span><h2 id="conversation-title">对话</h2></div>
            <button className="icon-button product-icon-button" type="button" title="清空对话" aria-label="清空对话" disabled={chat.exchanges.length === 0} onClick={() => void chat.clear()}>
              <Trash2 size={16} aria-hidden="true" />
            </button>
          </header>

          <div ref={conversationScroll} className="conversation-scroll" aria-label="主动对话历史">
            {chat.exchanges.length ? chat.exchanges.map((exchange) => <SpeechBubble key={exchange.key} exchange={exchange} />) : (
              <div className="conversation-empty">
                <span className="conversation-empty__check"><Check size={19} aria-hidden="true" /></span>
                <strong>可以开始聊了</strong>
                <p>这里还没有消息。</p>
              </div>
            )}
          </div>

          <footer className="conversation-composer">
            <div className="conversation-composer__controls">
              <MessageInput busy={chat.submitting} disabled={!chat.ready || !active || !available} onSend={chat.send} />
              <MicrophoneButton disabled={!active || !available} />
            </div>
          </footer>
        </section>
      </div>

      {scheduleOpen ? (
        <div className="schedule-dialog-backdrop" role="presentation" onMouseDown={(event) => {
          if (event.target === event.currentTarget && !scheduleBusy) setScheduleOpen(false);
        }}>
          <section className="schedule-dialog" role="dialog" aria-modal="true" aria-labelledby="schedule-title">
            <header className="schedule-dialog__header">
              <div><span>屏幕记录</span><h2 id="schedule-title">每日日程</h2></div>
              <div className="schedule-dialog__actions">
                <button className="window-command" type="button" disabled={scheduleBusy} onClick={() => void openTodaySchedule()}>
                  <CalendarDays size={14} aria-hidden="true" />{scheduleBusy ? "生成中" : "生成今天"}
                </button>
                <button className="icon-button product-icon-button" type="button" title="关闭日程总结" aria-label="关闭日程总结" onClick={() => setScheduleOpen(false)}><X size={17} aria-hidden="true" /></button>
              </div>
            </header>
            <div className="schedule-dialog__body">
              <aside className="schedule-list" aria-label="日程日期">
                {scheduleItems.length ? scheduleItems.map((item) => (
                  <button key={item.date} className={schedule?.date === item.date ? "schedule-list__item schedule-list__item--active" : "schedule-list__item"} type="button" onClick={() => void getDailySummary(item.date).then(setSchedule)}>
                    <strong>{item.date}</strong><span>{item.title}</span><small>{item.imageCount} 张精选截图</small>
                  </button>
                )) : <p className="schedule-empty">还没有已归档的日程。</p>}
              </aside>
              <article className="schedule-reader">
                {schedule ? <>
                  <header><div><span>{schedule.date}</span><h3>{schedule.title}</h3></div><button className="icon-button product-icon-button" type="button" title="删除这份日程" aria-label="删除这份日程" disabled={scheduleBusy} onClick={() => void removeSchedule()}><Trash2 size={16} aria-hidden="true" /></button></header>
                  <pre>{schedule.markdown.replace(/^# .*\n\n?/, "")}</pre>
                  <div className="schedule-images">{schedule.images.map((image) => <img key={image.name} src={image.dataUrl} alt={image.name} />)}</div>
                </> : <div className="schedule-empty schedule-empty--reader"><CalendarDays size={22} aria-hidden="true" /><p>选择日期查看日程总结。</p></div>}
              </article>
            </div>
          </section>
        </div>
      ) : null}

      {memoryOpen ? (
        <div className="schedule-dialog-backdrop" role="presentation" onMouseDown={(event) => {
          if (event.target === event.currentTarget && !memoryBusy) setMemoryOpen(false);
        }}>
          <section className="schedule-dialog" role="dialog" aria-modal="true" aria-labelledby="memory-title">
            <header className="schedule-dialog__header">
              <div><span>MEMORY</span><h2 id="memory-title">长期记忆</h2></div>
              <div className="schedule-dialog__actions">
                <button className="window-command" type="button" disabled={memoryBusy} onClick={() => void toggleMemory()}><Brain size={14} aria-hidden="true" />{memoryEnabled ? "已启用" : "已停用"}</button>
                <button className="icon-button product-icon-button" type="button" title="关闭记忆管理" aria-label="关闭记忆管理" onClick={() => setMemoryOpen(false)}><X size={17} aria-hidden="true" /></button>
              </div>
            </header>
            <div className="schedule-dialog__body">
              <aside className="schedule-list" aria-label="记忆列表">
                {memories.length ? memories.map((item) => (
                  <div key={item.id} className="schedule-list__item">
                    <strong>{item.kind === "preference" ? "偏好" : item.kind === "fact" ? "事实" : "任务"}</strong>
                    <span>{item.text}</span>
                    <button className="icon-button product-icon-button" type="button" title="编辑记忆" aria-label="编辑记忆" disabled={memoryBusy} onClick={() => setMemoryDraft(item)}><Pencil size={14} aria-hidden="true" /></button>
                    <button className="icon-button product-icon-button" type="button" title="删除记忆" aria-label="删除记忆" disabled={memoryBusy} onClick={() => void removeMemory(item.id)}><Trash2 size={14} aria-hidden="true" /></button>
                  </div>
                )) : <p className="schedule-empty">系统还没有从对话中提炼出记忆。</p>}
              </aside>
              <article className="schedule-reader">
                {memoryDraft ? <form onSubmit={(event) => void saveMemory(event)}>
                  <label><span>类型</span><select value={memoryDraft.kind} onChange={(event) => setMemoryDraft({ ...memoryDraft, kind: event.currentTarget.value as MemoryKind })}><option value="preference">偏好</option><option value="fact">事实</option><option value="task">任务背景</option></select></label>
                  <label><span>内容</span><textarea required maxLength={512} rows={6} value={memoryDraft.text} onChange={(event) => setMemoryDraft({ ...memoryDraft, text: event.currentTarget.value })} /></label>
                  <button className="window-command" type="submit" disabled={memoryBusy}><Check size={14} aria-hidden="true" />保存修改</button>
                  <button className="window-command" type="button" disabled={memoryBusy} onClick={() => setMemoryDraft(undefined)}><X size={14} aria-hidden="true" />取消</button>
                </form> : <div className="schedule-empty schedule-empty--reader"><Brain size={22} aria-hidden="true" /><p>记忆会从明确的对话表达中自动提炼。</p><small>例如“我喜欢简洁的代码”或“记住我在上海”。</small></div>}
              </article>
            </div>
          </section>
        </div>
      ) : null}

      {draft ? (
        <div className="persona-dialog-backdrop" role="presentation" onMouseDown={(event) => {
          if (event.target === event.currentTarget && !personaPending) setDraft(undefined);
        }}>
          <section className="persona-dialog" role="dialog" aria-modal="true" aria-labelledby="persona-dialog-title">
            <header>
              <div><span>PERSONA</span><h2 id="persona-dialog-title">{draft.id ? "编辑人设" : "新建人设"}</h2></div>
              <button type="button" title="关闭" aria-label="关闭" disabled={personaPending} onClick={() => setDraft(undefined)}><X size={18} aria-hidden="true" /></button>
            </header>
            <form onSubmit={(event) => void savePersona(event)}>
              <label>
                <span>名称</span>
                <input autoFocus maxLength={32} required value={draft.name} onChange={(event) => setDraft({ ...draft, name: event.currentTarget.value })} placeholder="例如：代码搭档" />
              </label>
              <label>
                <span>人设描述</span>
                <textarea maxLength={8000} required rows={9} value={draft.prompt} onChange={(event) => setDraft({ ...draft, prompt: event.currentTarget.value })} placeholder="描述它的性格、表达方式、兴趣与边界" />
              </label>
              <div className="persona-dialog__footer">
                <button type="button" onClick={() => setDraft(undefined)} disabled={personaPending}>取消</button>
                <button type="submit" disabled={personaPending || !draft.name.trim() || !draft.prompt.trim()}>{personaPending ? "正在保存" : "保存人设"}</button>
              </div>
            </form>
          </section>
        </div>
      ) : null}
    </main>
  );
}
