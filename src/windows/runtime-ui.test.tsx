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
  clearChatHistory: vi.fn(),
  getBootstrapState: vi.fn(),
  getChatHistory: vi.fn(),
  getRuntimeState: vi.fn(),
  getActivityState: vi.fn(),
  noteUserActivity: vi.fn(),
  listenChatStatus: vi.fn(),
  listenClearTransientOutput: vi.fn(),
  listenObservationResult: vi.fn(),
  listenRuntimeState: vi.fn(),
  quitApp: vi.fn(),
  sendTextMessage: vi.fn(),
  setFeatureEnabled: vi.fn(),
  setPrivacyPaused: vi.fn(),
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
      systemAudioEnabled: false,
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

describe("AG-02 runtime UI", () => {
  let container: HTMLDivElement;
  let root: Root;
  let runtimeHandler: ((state: RuntimeSnapshot) => void) | undefined;
  let chatHandlers: Array<(event: ChatStatusEvent) => void>;
  let clearHandlers: Array<(event: ClearTransientOutputEvent) => void>;
  let observationHandlers: Array<(event: ObservationResultEvent) => void>;
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
    ipcMocks.getActivityState.mockResolvedValue({ epoch: 1, mode: "focused", scene: "coding", inputIdleSeconds: 0, regularCheckSeconds: 160, modelCalls: 5, suppressedResponses: 0, successfulChecks: 3, silentChecks: 2, failedChecks: 1, modelBusy: false, lastCompletedAgoSeconds: 12 });
    ipcMocks.noteUserActivity.mockResolvedValue(undefined);
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
    ipcMocks.getBootstrapState.mockResolvedValue(bootstrap);
    ipcMocks.getRuntimeState.mockResolvedValue(snapshot(1));
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

  it("loads runtime state, follows newer events, and redacts diagnostic details", async () => {
    ipcMocks.getRuntimeState.mockResolvedValue(
      snapshot(2, {
        diagnostics: [
          {
            source: "settings",
            kind: "readFailed",
            message: "C:\\Users\\private\\settings.json contains a secret",
          },
        ],
      }),
    );

    await act(async () => root.render(<ControlWindow />));
    await flushEffects();

    expect(container.textContent).toContain("本地运行");
    expect(container.textContent).toContain("观察调用 5 次");
    expect(container.textContent).toContain("完成 3 次 · 未投递 2 次 · 失败 1 次");
    expect(container.textContent).toContain("最近完成于 12 秒前");
    expect(container.textContent).toContain("功能设置");
    expect(container.textContent).toContain("读取失败，已使用保守默认值");
    expect(container.textContent).not.toContain("private");
    expect(findSwitch(container, "屏幕感知").checked).toBe(true);
    expect(findSwitch(container, "系统声音").checked).toBe(false);

    act(() => runtimeHandler?.(snapshot(3, { privacyState: "paused" })));
    expect(container.textContent).toContain("已暂停");
    expect(findSwitch(container, "隐私暂停").checked).toBe(true);
    expect(container.querySelector('[role="status"]')?.textContent).toContain(
      "隐私暂停已开启",
    );

    act(() => runtimeHandler?.(snapshot(1, { privacyState: "active" })));
    expect(findSwitch(container, "隐私暂停").checked).toBe(true);

    act(() => {
      for (const handler of clearHandlers) {
        handler({ revision: 4, runtimeGeneration: 4, reason: "privacyPaused" });
      }
    });
    expect(container.querySelector("main")?.getAttribute("data-transient-clear-revision")).toBe("4");
  });

  it("serializes runtime mutations and disables every switch while pending", async () => {
    let finishPrivacy: ((value: RuntimeSnapshot) => void) | undefined;
    ipcMocks.setPrivacyPaused.mockReturnValue(
      new Promise<RuntimeSnapshot>((resolve) => {
        finishPrivacy = resolve;
      }),
    );

    await act(async () => root.render(<ControlWindow />));
    await flushEffects();

    const privacy = findSwitch(container, "隐私暂停");
    act(() => privacy.click());

    expect(ipcMocks.setPrivacyPaused).toHaveBeenCalledTimes(1);
    expect(ipcMocks.setPrivacyPaused).toHaveBeenCalledWith(true);
    expect(
      Array.from(container.querySelectorAll<HTMLInputElement>('input[role="switch"]')).every(
        (input) => input.disabled,
      ),
    ).toBe(true);
    expect(container.querySelector('[role="status"]')?.textContent).toContain(
      "正在更新隐私暂停",
    );

    act(() => findSwitch(container, "屏幕感知").click());
    expect(ipcMocks.setFeatureEnabled).not.toHaveBeenCalled();

    await act(async () => finishPrivacy?.(snapshot(2, { privacyState: "paused" })));
    expect(findSwitch(container, "隐私暂停").checked).toBe(true);
    expect(findSwitch(container, "屏幕感知").disabled).toBe(false);
    expect(container.querySelector('[role="status"]')?.textContent).toContain(
      "隐私暂停已开启",
    );
  });

  it("clears only runtime action errors when a newer snapshot arrives", async () => {
    ipcMocks.setFeatureEnabled.mockRejectedValue({
      code: "configWrite",
      message: "C:\\Users\\private\\settings.json",
    });

    await act(async () => root.render(<ControlWindow />));
    await flushEffects();

    await act(async () => findSwitch(container, "系统声音").click());
    await flushEffects();
    expect(container.textContent).toContain("无法保存本地配置");
    expect(container.textContent).not.toContain("private");

    act(() => runtimeHandler?.(snapshot(2)));
    expect(container.textContent).not.toContain("无法保存本地配置");

    ipcMocks.showAppWindow.mockRejectedValue(
      "C:\\Users\\private\\window-state.json",
    );
    const openWindow = Array.from(container.querySelectorAll("button")).find(
      (button) => button.textContent?.trim() === "打开",
    );
    if (!openWindow) throw new Error("missing open window button");
    await act(async () => openWindow.click());
    await flushEffects();
    expect(container.textContent).toContain("本地服务没有返回可用信息");
    expect(container.textContent).not.toContain("private");

    act(() => runtimeHandler?.(snapshot(3)));
    expect(container.textContent).toContain("本地服务没有返回可用信息");
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
