import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type {
  BootstrapState,
  ChatStatusEvent,
  ClearTransientOutputEvent,
  ObservationResultEvent,
  RuntimeSnapshot,
} from "../lib/types";

const ipcMocks = vi.hoisted(() => ({
  closeAppWindow: vi.fn(),
  createPersona: vi.fn(),
  deletePersona: vi.fn(),
  clearChatHistory: vi.fn(),
  getBootstrapState: vi.fn(),
  getChatHistory: vi.fn(),
  getRuntimeState: vi.fn(),
  getPersonas: vi.fn(),
  getActivityState: vi.fn(),
  noteUserActivity: vi.fn(),
  listenChatStatus: vi.fn(),
  listenClearTransientOutput: vi.fn(),
  listenCourseNoteReady: vi.fn(),
  listenCourseNoteFailed: vi.fn(),
  listenObservationResult: vi.fn(),
  listenRuntimeState: vi.fn(),
  quitApp: vi.fn(),
  respondCourseConfirmation: vi.fn(),
  sendTextMessage: vi.fn(),
  setFeatureEnabled: vi.fn(),
  setPrivacyPaused: vi.fn(),
  selectPersona: vi.fn(),
  updatePersona: vi.fn(),
  showAppWindow: vi.fn(),
  startCurrentWindowDragging: vi.fn(),
}));

vi.mock("../lib/ipc", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/ipc")>()),
  ...ipcMocks,
}));

vi.mock("../components/PetSprite", () => ({
  PetSprite: ({
    state,
    animated = false,
    replayKey = 0,
  }: {
    state: string;
    animated?: boolean;
    replayKey?: number | string;
  }) => (
    <div
      data-pet-sprite
      data-state={state}
      data-animated={String(animated)}
      data-replay-key={String(replayKey)}
    />
  ),
}));

import { ControlWindow } from "./ControlWindow";
import { PetWindow } from "./PetWindow";

const bootstrap: BootstrapState = {
  appVersion: "0.1.0",
  currentWindow: "control",
  resourceStatus: {
    modelConfig: true,
    defaultsConfig: true,
    petManifest: true,
  },
  windows: [
    { kind: "control", open: true },
    { kind: "pet", open: true },
    { kind: "danmaku", open: false },
  ],
};

function snapshot(
  revision: number,
  overrides: Partial<RuntimeSnapshot> = {},
): RuntimeSnapshot {
  return {
    revision,
    runtimeGeneration: revision,
    privacyState: "active",
    lifecycleState: "running",
    features: {
      screenEnabled: true,
      systemAudioEnabled: true,
      proactiveResponseEnabled: true,
    },
    diagnostics: [],
    ...overrides,
  };
}

async function flushEffects() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

function findSwitch(container: HTMLElement, label: string): HTMLInputElement {
  const result = Array.from(
    container.querySelectorAll<HTMLInputElement>('input[role="switch"]'),
  ).find((input) => input.closest("label")?.textContent?.includes(label));
  if (!result) throw new Error(`missing switch: ${label}`);
  return result;
}

function installMatchMedia(matches: boolean): MediaQueryList {
  const mediaQuery = {
    matches,
    media: "(prefers-reduced-motion: reduce)",
    onchange: null,
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    addListener: vi.fn(),
    removeListener: vi.fn(),
    dispatchEvent: vi.fn(),
  } as unknown as MediaQueryList;
  Object.defineProperty(window, "matchMedia", {
    configurable: true,
    value: vi.fn().mockReturnValue(mediaQuery),
  });
  return mediaQuery;
}

describe("runtime UI", () => {
  let container: HTMLDivElement;
  let root: Root;
  let runtimeHandler: ((state: RuntimeSnapshot) => void) | undefined;
  let chatHandlers: Array<(event: ChatStatusEvent) => void>;
  let clearHandlers: Array<(event: ClearTransientOutputEvent) => void>;
  let observationHandlers: Array<(event: ObservationResultEvent) => void>;
  let courseNoteHandlers: Array<(event: { title: string; path: string; message: string }) => void>;
  let runtimeUnlisten: ReturnType<typeof vi.fn>;
  let clearUnlisten: ReturnType<typeof vi.fn>;
  let chatUnlisten: ReturnType<typeof vi.fn>;
  let observationUnlisten: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    (
      globalThis as typeof globalThis & {
        IS_REACT_ACT_ENVIRONMENT: boolean;
      }
    ).IS_REACT_ACT_ENVIRONMENT = true;
    for (const mock of Object.values(ipcMocks)) mock.mockReset();
    ipcMocks.getActivityState.mockResolvedValue({ epoch: 1, mode: "focused", scene: "other", inputIdleSeconds: 0, regularCheckSeconds: 160, modelCalls: 5, suppressedResponses: 0, successfulChecks: 3, silentChecks: 2, failedChecks: 1, modelBusy: false, lastCompletedAgoSeconds: 12 });
    ipcMocks.noteUserActivity.mockResolvedValue(undefined);
    ipcMocks.respondCourseConfirmation.mockResolvedValue(undefined);
    installMatchMedia(false);
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
    runtimeUnlisten = vi.fn();
    clearUnlisten = vi.fn();
    chatUnlisten = vi.fn();
    observationUnlisten = vi.fn();
    clearHandlers = [];
    chatHandlers = [];
    observationHandlers = [];
    courseNoteHandlers = [];
    ipcMocks.getBootstrapState.mockResolvedValue(bootstrap);
    ipcMocks.getRuntimeState.mockResolvedValue(snapshot(1));
    ipcMocks.getPersonas.mockResolvedValue({ activeId: "default", personas: [{ id: "default", name: "默认伙伴", prompt: "自然、直接。" }] });
    ipcMocks.createPersona.mockResolvedValue({ activeId: "persona-1", personas: [{ id: "default", name: "默认伙伴", prompt: "自然、直接。" }, { id: "persona-1", name: "代码搭档", prompt: "简洁、严谨。" }] });
    ipcMocks.deletePersona.mockResolvedValue({ activeId: "default", personas: [{ id: "default", name: "默认伙伴", prompt: "自然、直接。" }] });
    ipcMocks.selectPersona.mockResolvedValue({ activeId: "default", personas: [{ id: "default", name: "默认伙伴", prompt: "自然、直接。" }] });
    ipcMocks.updatePersona.mockResolvedValue({ activeId: "default", personas: [{ id: "default", name: "默认伙伴", prompt: "自然、直接。" }] });
    ipcMocks.getChatHistory.mockResolvedValue([]);
    ipcMocks.listenChatStatus.mockImplementation(
      async (handler: (event: ChatStatusEvent) => void) => {
        chatHandlers.push(handler);
        return chatUnlisten;
      },
    );
    ipcMocks.listenObservationResult.mockImplementation(
      async (handler: (event: ObservationResultEvent) => void) => {
        observationHandlers.push(handler);
        return observationUnlisten;
      },
    );
    ipcMocks.listenCourseNoteReady.mockImplementation(
      async (handler: (event: { title: string; path: string; message: string }) => void) => {
        courseNoteHandlers.push(handler);
        return vi.fn();
      },
    );
    ipcMocks.listenCourseNoteFailed.mockResolvedValue(vi.fn());
    ipcMocks.listenRuntimeState.mockImplementation(
      async (handler: (state: RuntimeSnapshot) => void) => {
        runtimeHandler = handler;
        return runtimeUnlisten;
      },
    );
    ipcMocks.listenClearTransientOutput.mockImplementation(
      async (handler: (event: ClearTransientOutputEvent) => void) => {
        clearHandlers.push(handler);
        return clearUnlisten;
      },
    );
    ipcMocks.closeAppWindow.mockResolvedValue(undefined);
    ipcMocks.showAppWindow.mockResolvedValue(undefined);
    ipcMocks.startCurrentWindowDragging.mockResolvedValue(undefined);
    ipcMocks.quitApp.mockResolvedValue(undefined);
    ipcMocks.setPrivacyPaused.mockResolvedValue(
      snapshot(2, { privacyState: "paused" }),
    );
    ipcMocks.setFeatureEnabled.mockResolvedValue(snapshot(2));
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    vi.useRealTimers();
  });

  it("renders the product conversation and persona surfaces without development controls", async () => {
    await act(async () => root.render(<ControlWindow />));
    await flushEffects();

    expect(container.textContent).toContain("默认伙伴");
    expect(container.textContent).toContain("对话");
    expect(container.textContent).toContain("正在陪伴");
    expect(container.textContent).not.toContain("本地资源");
    expect(container.textContent).not.toContain("屏幕感知");
    expect(container.textContent).not.toContain("观察调用");
    expect(observationHandlers).toHaveLength(0);

    const pause = Array.from(container.querySelectorAll("button")).find((button) =>
      button.textContent?.includes("暂停陪伴"),
    );
    if (!pause) throw new Error("missing presence toggle");
    await act(async () => pause.click());
    expect(ipcMocks.setPrivacyPaused).toHaveBeenCalledWith(true);
  });

  it("restores every required feature when continuing from a partial legacy state", async () => {
    ipcMocks.getRuntimeState.mockResolvedValue(snapshot(1, {
      features: {
        screenEnabled: true,
        systemAudioEnabled: false,
        proactiveResponseEnabled: true,
      },
    }));
    await act(async () => root.render(<ControlWindow />));
    await flushEffects();

    const resume = Array.from(container.querySelectorAll("button")).find((button) =>
      button.textContent?.includes("继续陪伴"),
    );
    if (!resume) throw new Error("missing resume button");
    await act(async () => resume.click());

    expect(ipcMocks.setFeatureEnabled.mock.calls).toEqual([
      ["screen", true],
      ["systemAudio", true],
      ["proactiveResponse", true],
    ]);
    expect(ipcMocks.setPrivacyPaused).toHaveBeenCalledWith(false);
  });

  it("creates a persona from the bounded editor and makes it current", async () => {
    await act(async () => root.render(<ControlWindow />));
    await flushEffects();

    const create = container.querySelector<HTMLButtonElement>('button[aria-label="新建人设"]');
    if (!create) throw new Error("missing create persona button");
    act(() => create.click());

    const name = container.querySelector<HTMLInputElement>('input[maxlength="32"]');
    const prompt = container.querySelector<HTMLTextAreaElement>('textarea[maxlength="8000"]');
    if (!name || !prompt) throw new Error("missing persona fields");
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set?.call(name, "代码搭档");
      name.dispatchEvent(new Event("input", { bubbles: true }));
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value")?.set?.call(prompt, "简洁、严谨。");
      prompt.dispatchEvent(new Event("input", { bubbles: true }));
    });
    const save = Array.from(container.querySelectorAll<HTMLButtonElement>("button")).find(
      (button) => button.textContent === "保存人设",
    );
    if (!save) throw new Error("missing save persona button");
    await act(async () => save.click());
    await flushEffects();

    expect(ipcMocks.createPersona).toHaveBeenCalledWith("代码搭档", "简洁、严谨。");
    expect(container.textContent).toContain("代码搭档");
  });

  it("keeps runtime and window action errors separate in the pet", async () => {
    ipcMocks.getRuntimeState.mockRejectedValue({
      code: "internal",
      message: "C:\\Users\\private\\runtime.log",
    });

    await act(async () => root.render(<PetWindow />));
    await flushEffects();

    expect(container.textContent).toContain("本地运行时暂时不可用");
    expect(container.textContent).not.toContain("private");

    const openControl = container.querySelector<HTMLButtonElement>(
      'button[aria-label="打开控制台"]',
    );
    if (!openControl) throw new Error("missing open control button");
    await act(async () => openControl.click());
    await flushEffects();
    expect(container.textContent).toContain("本地运行时暂时不可用");

    act(() => runtimeHandler?.(snapshot(2)));
    expect(container.textContent).not.toContain("本地运行时暂时不可用");
    expect(container.querySelector('[role="status"]')?.textContent).toContain(
      "桌宠处于本地运行状态",
    );

    ipcMocks.showAppWindow.mockRejectedValue(
      "C:\\Users\\private\\window-state.json",
    );
    await act(async () => openControl.click());
    await flushEffects();
    expect(container.textContent).toContain("本地服务没有返回可用信息");
    expect(container.textContent).not.toContain("private");

    act(() => runtimeHandler?.(snapshot(3)));
    expect(container.textContent).toContain("本地服务没有返回可用信息");
  });

  it("starts dragging the pet window only with the primary mouse button", async () => {
    await act(async () => root.render(<PetWindow />));
    await flushEffects();

    const dragRegion = container.querySelector<HTMLElement>(
      "[data-pet-drag-region]",
    );
    if (!dragRegion) throw new Error("missing pet drag region");

    act(() => {
      dragRegion.dispatchEvent(
        new MouseEvent("mousedown", { bubbles: true, button: 2 }),
      );
    });
    expect(ipcMocks.startCurrentWindowDragging).not.toHaveBeenCalled();

    act(() => {
      dragRegion.dispatchEvent(
        new MouseEvent("mousedown", { bubbles: true, button: 0 }),
      );
    });
    expect(ipcMocks.startCurrentWindowDragging).toHaveBeenCalledOnce();

    act(() => {
      dragRegion.dispatchEvent(
        new MouseEvent("mousedown", { bubbles: true, button: 0, detail: 2 }),
      );
    });
    expect(ipcMocks.startCurrentWindowDragging).toHaveBeenCalledOnce();
  });

  it("toggles privacy mode when the pet is double-clicked", async () => {
    await act(async () => root.render(<PetWindow />));
    await flushEffects();

    const dragRegion = container.querySelector<HTMLElement>(
      "[data-pet-drag-region]",
    );
    if (!dragRegion) throw new Error("missing pet drag region");

    await act(async () => {
      dragRegion.dispatchEvent(
        new MouseEvent("dblclick", { bubbles: true, button: 0, detail: 2 }),
      );
    });
    expect(ipcMocks.setPrivacyPaused).toHaveBeenCalledOnce();
    expect(ipcMocks.setPrivacyPaused).toHaveBeenCalledWith(true);

    act(() => runtimeHandler?.(snapshot(2, { privacyState: "paused" })));
    await act(async () => {
      dragRegion.dispatchEvent(
        new MouseEvent("dblclick", { bubbles: true, button: 0, detail: 2 }),
      );
    });
    expect(ipcMocks.setPrivacyPaused).toHaveBeenLastCalledWith(false);
  });

  it("keeps the pet input hidden until Control+M toggles it", async () => {
    await act(async () => root.render(<PetWindow />));
    await flushEffects();

    const input = () =>
      container.querySelector<HTMLTextAreaElement>(
        'textarea[aria-label="发送文字消息"]',
      );
    expect(input()).toBeNull();
    expect(container.querySelector("main")?.dataset.inputVisible).toBe("false");

    act(() => {
      window.dispatchEvent(
        new KeyboardEvent("keydown", { key: "m", bubbles: true }),
      );
    });
    expect(input()).toBeNull();

    act(() => {
      window.dispatchEvent(
        new KeyboardEvent("keydown", {
          key: "m",
          ctrlKey: true,
          bubbles: true,
        }),
      );
    });
    expect(input()).not.toBeNull();
    expect(container.querySelector("main")?.dataset.inputVisible).toBe("true");

    act(() => {
      window.dispatchEvent(
        new KeyboardEvent("keydown", {
          key: "M",
          ctrlKey: true,
          bubbles: true,
        }),
      );
    });
    expect(input()).toBeNull();
    expect(ipcMocks.getChatHistory).toHaveBeenCalledOnce();
    expect(ipcMocks.clearChatHistory).not.toHaveBeenCalled();
  });

  it("keeps a pet reply readable for ten seconds and resets for new text", async () => {
    vi.useFakeTimers();
    await act(async () => root.render(<PetWindow />));
    await flushEffects();

    const emitReply = (requestId: number, reply: string) => {
      act(() => {
        for (const handler of chatHandlers) {
          handler({
            requestId,
            runtimeGeneration: 1,
            status: "replied",
            userMessage: "question",
            reply,
            failure: null,
          });
        }
      });
    };
    const bubble = () => container.querySelector(".pet-chat .speech-bubble");

    emitReply(1, "first reply");
    expect(bubble()?.textContent).toContain("first reply");

    act(() => vi.advanceTimersByTime(9_999));
    expect(bubble()?.textContent).toContain("first reply");

    emitReply(2, "second reply");
    act(() => vi.advanceTimersByTime(9_999));
    expect(bubble()?.textContent).toContain("second reply");

    act(() => vi.advanceTimersByTime(1));
    expect(bubble()).toBeNull();
  });

  it("uses static pet assets when reduced motion is requested", async () => {
    installMatchMedia(true);
    vi.useFakeTimers();

    await act(async () => root.render(<PetWindow />));
    await flushEffects();

    const pet = () => container.querySelector<HTMLElement>("[data-pet-sprite]");
    expect(pet()?.dataset.state).toBe("idle");
    expect(pet()?.dataset.animated).toBe("false");

    act(() => runtimeHandler?.(snapshot(5, { privacyState: "paused" })));
    expect(pet()?.dataset.state).toBe("paused");
    expect(pet()?.dataset.animated).toBe("false");
  });

  it("shows the course asset for lessons and privacy pause overrides it", async () => {
    ipcMocks.getActivityState.mockResolvedValue({ epoch: 1, mode: "media", scene: "course", successfulChecks: 1 });
    await act(async () => root.render(<PetWindow />));
    await flushEffects();
    const pet = () => container.querySelector<HTMLElement>("[data-pet-sprite]");
    expect(pet()?.dataset.state).toBe("course");
    act(() => runtimeHandler?.(snapshot(5, { privacyState: "paused" })));
    expect(pet()?.dataset.state).toBe("paused");
  });

  it("asks before leaving a course and honors the user's choice", async () => {
    ipcMocks.getActivityState.mockResolvedValue({
      epoch: 1,
      mode: "media",
      scene: "other",
      activeSession: "course",
      courseConfirmationRemainingSeconds: 10,
      courseCompletionId: 0,
      successfulChecks: 1,
    });
    await act(async () => root.render(<PetWindow />));
    await flushEffects();

    const dialog = container.querySelector('[role="alertdialog"]');
    expect(dialog?.textContent).toContain("你还在上课吗？");
    expect(dialog?.textContent).toContain("10 秒后自动结束");
    const keep = Array.from(dialog?.querySelectorAll("button") ?? []).find(
      (button) => button.textContent === "继续上课",
    );
    await act(async () => keep?.click());
    expect(ipcMocks.respondCourseConfirmation).toHaveBeenCalledWith(true);
    expect(container.querySelector<HTMLElement>("[data-pet-sprite]")?.dataset.state).toBe("course");
  });

  it("shows a temporary bubble when a course note is ready", async () => {
    vi.useFakeTimers();
    await act(async () => root.render(<PetWindow />));
    await flushEffects();

    act(() => {
      for (const handler of courseNoteHandlers) {
        handler({
          title: "矩阵与变换",
          path: "C:\\Users\\test\\Desktop\\AI Jarvis 课程笔记\\笔记.md",
          message: "刚才的网课笔记已经整理好，放到桌面啦。",
        });
      }
    });
    expect(container.querySelector(".pet-chat")?.textContent).toContain("网课笔记已经整理好");

    act(() => vi.advanceTimersByTime(10_000));
    expect(container.querySelector(".pet-chat")?.textContent).not.toContain("网课笔记已经整理好");
  });

  it("applies observed scenes immediately and keeps course ahead of reply animation", async () => {
    ipcMocks.getActivityState.mockResolvedValue({
      epoch: 1,
      mode: "transitioning",
      scene: "course",
      successfulChecks: 1,
    });
    await act(async () => root.render(<PetWindow />));
    await flushEffects();
    const pet = () => container.querySelector<HTMLElement>("[data-pet-sprite]");

    act(() => {
      for (const handler of observationHandlers) {
        handler({
          runtimeRevision: 2,
          runtimeGeneration: 2,
          latestFrameSequence: 10,
          scene: "other",
          needResponse: true,
          nextCheckSeconds: 30,
          message: "正在编写文档。",
        });
      }
    });
    expect(pet()?.dataset.state).toBe("normal");

    act(() => {
      for (const handler of observationHandlers) {
        handler({
          runtimeRevision: 2,
          runtimeGeneration: 2,
          latestFrameSequence: 11,
          scene: "course",
          needResponse: true,
          nextCheckSeconds: 30,
          message: "正在看高等数学课程。",
        });
      }
    });
    expect(pet()?.dataset.state).toBe("course");

    act(() => {
      for (const handler of observationHandlers) {
        handler({
          runtimeRevision: 2,
          runtimeGeneration: 2,
          latestFrameSequence: 12,
          scene: "other",
          needResponse: false,
          nextCheckSeconds: 5,
          message: "",
        });
      }
    });
    expect(pet()?.dataset.state).toBe("normal");
  });

  it("updates the pet to course on a silent observation", async () => {
    ipcMocks.getActivityState.mockResolvedValue({ epoch: 1, mode: "available", scene: "other", successfulChecks: 1 });
    await act(async () => root.render(<PetWindow />));
    await flushEffects();
    act(() => {
      for (const handler of observationHandlers) {
        handler({ runtimeRevision: 2, runtimeGeneration: 2, latestFrameSequence: 10,
          scene: "course", needResponse: false, nextCheckSeconds: 45, message: "" });
      }
    });
    expect(container.querySelector<HTMLElement>("[data-pet-sprite]")?.dataset.state).toBe("course");
  });

  it("shows a course observation in a bubble and plays the course animation once", async () => {
    vi.useFakeTimers();
    ipcMocks.getActivityState.mockResolvedValue({
      epoch: 1,
      mode: "media",
      scene: "course",
      activeSession: "course",
      courseConfirmationRemainingSeconds: null,
      courseCompletionId: 0,
      successfulChecks: 1,
    });
    await act(async () => root.render(<PetWindow />));
    await flushEffects();

    act(() => {
      for (const handler of observationHandlers) {
        handler({
          runtimeRevision: 2,
          runtimeGeneration: 2,
          latestFrameSequence: 15,
          scene: "course",
          needResponse: true,
          nextCheckSeconds: 60,
          message: "这里的条件很容易漏，是个典型踩坑点。",
        });
      }
    });
    expect(container.querySelector(".pet-chat")?.textContent).toContain("典型踩坑点");
    const pet = () => container.querySelector<HTMLElement>("[data-pet-sprite]");
    expect(pet()?.dataset.state).toBe("course");
    expect(pet()?.dataset.animated).toBe("true");

    act(() => vi.advanceTimersByTime(2_500));
    expect(pet()?.dataset.state).toBe("course");
    expect(pet()?.dataset.animated).toBe("false");
  });

  it("keeps quiet presence local and stops activity polling on privacy pause", async () => {
    vi.useFakeTimers();
    ipcMocks.getRuntimeState.mockResolvedValue(snapshot(4));
    await act(async () => root.render(<PetWindow />));
    await flushEffects();
    expect(ipcMocks.getActivityState).toHaveBeenCalled();
    const pet = () => container.querySelector<HTMLElement>("[data-pet-sprite]");
    const initialReplay = Number(pet()?.dataset.replayKey);
    let poll = 0;
    ipcMocks.getActivityState.mockImplementation(async () => ({ epoch: ++poll,
      mode: poll % 2 ? "interacting" : "transitioning", successfulChecks: 3 }));
    await act(async () => vi.advanceTimersByTimeAsync(15_000));
    expect(Number(pet()?.dataset.replayKey)).toBeGreaterThan(initialReplay);
    expect(pet()?.dataset.animated).toBe("true");
    expect(ipcMocks.sendTextMessage).not.toHaveBeenCalled();
    await act(async () => vi.advanceTimersByTimeAsync(2_500));
    expect(pet()?.dataset.animated).toBe("false");
    act(() => runtimeHandler?.(snapshot(5, { privacyState: "paused" })));
    const calls = ipcMocks.getActivityState.mock.calls.length;
    await act(async () => vi.advanceTimersByTimeAsync(50_000));
    expect(ipcMocks.getActivityState).toHaveBeenCalledTimes(calls);
    expect(pet()?.dataset.state).toBe("paused");
  });

  it("moves the pet through paused animation, paused still, and idle", async () => {
    vi.useFakeTimers();
    await act(async () => root.render(<PetWindow />));
    await flushEffects();

    const pet = () => container.querySelector<HTMLElement>("[data-pet-sprite]");
    expect(pet()?.dataset.state).toBe("idle");
    expect(pet()?.dataset.animated).toBe("true");

    act(() => runtimeHandler?.(snapshot(5, { privacyState: "paused" })));
    expect(pet()?.dataset.state).toBe("paused");
    expect(pet()?.dataset.animated).toBe("true");
    expect(pet()?.dataset.replayKey).toBe("5");
    expect(container.querySelector("main")?.dataset.runtimeState).toBe("paused");

    act(() => vi.advanceTimersByTime(2_500));
    expect(pet()?.dataset.animated).toBe("false");

    act(() => runtimeHandler?.(snapshot(6)));
    expect(pet()?.dataset.state).toBe("idle");
    expect(pet()?.dataset.animated).toBe("true");

    act(() => {
      for (const handler of clearHandlers) {
        handler({ revision: 7, runtimeGeneration: 7, reason: "shutdown" });
      }
    });
    expect(container.querySelector("main")?.dataset.transientClearRevision).toBe("7");
  });

  it("plays one normal interaction for a game batch and returns to idle still", async () => {
    vi.useFakeTimers();
    await act(async () => root.render(<PetWindow />));
    await flushEffects();

    const pet = () => container.querySelector<HTMLElement>("[data-pet-sprite]");
    act(() => {
      for (const handler of observationHandlers) {
        handler({
          runtimeRevision: 2,
          runtimeGeneration: 2,
          latestFrameSequence: 9,
          scene: "game",
          needResponse: true,
          nextCheckSeconds: 5,
          message: "一\n二\n三",
        });
      }
    });

    expect(pet()?.dataset.state).toBe("normal");
    expect(pet()?.dataset.animated).toBe("true");
    expect(pet()?.dataset.replayKey).toBe("game-2-2-9");
    expect(container.textContent).not.toContain("一\n二\n三");

    act(() => vi.advanceTimersByTime(2_500));
    expect(pet()?.dataset.state).toBe("idle");
    expect(pet()?.dataset.animated).toBe("false");
  });

  it("unsubscribes every runtime and chat event handler", async () => {
    await act(async () => root.render(<PetWindow />));
    await flushEffects();
    await act(async () => root.unmount());
    root = createRoot(container);

    expect(runtimeUnlisten).toHaveBeenCalledOnce();
    expect(clearUnlisten).toHaveBeenCalledTimes(2);
    expect(chatUnlisten).toHaveBeenCalledOnce();
    expect(observationUnlisten).toHaveBeenCalledTimes(2);
  });
});
