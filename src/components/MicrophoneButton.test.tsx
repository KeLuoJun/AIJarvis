import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { VoiceInputState } from "../lib/types";

const ipcMocks = vi.hoisted(() => ({
  handlers: [] as Array<(state: VoiceInputState) => void>,
  unlisten: vi.fn(),
  getVoiceInputState: vi.fn(),
  startVoiceInput: vi.fn(),
  stopVoiceInput: vi.fn(),
  cancelVoiceInput: vi.fn(),
}));

vi.mock("../lib/ipc", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/ipc")>()),
  getVoiceInputState: ipcMocks.getVoiceInputState,
  startVoiceInput: ipcMocks.startVoiceInput,
  stopVoiceInput: ipcMocks.stopVoiceInput,
  cancelVoiceInput: ipcMocks.cancelVoiceInput,
  listenVoiceInputStatus: vi.fn(async (handler: (state: VoiceInputState) => void) => {
    ipcMocks.handlers.push(handler);
    return ipcMocks.unlisten;
  }),
}));

import { MicrophoneButton } from "./MicrophoneButton";

const state = (
  status: VoiceInputState["status"],
  revision: number,
  requestId: number | null = 1,
): VoiceInputState => ({ status, revision, requestId });

async function flushEffects() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

describe("AG-06 microphone control", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (
      globalThis as typeof globalThis & {
        IS_REACT_ACT_ENVIRONMENT: boolean;
      }
    ).IS_REACT_ACT_ENVIRONMENT = true;
    ipcMocks.handlers.length = 0;
    ipcMocks.unlisten.mockReset();
    ipcMocks.getVoiceInputState.mockReset().mockResolvedValue(state("idle", 0, null));
    ipcMocks.startVoiceInput.mockReset().mockResolvedValue(state("recording", 1));
    ipcMocks.stopVoiceInput.mockReset().mockResolvedValue(state("processing", 2));
    ipcMocks.cancelVoiceInput.mockReset().mockResolvedValue(state("cancelled", 2));
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    vi.useRealTimers();
  });

  it("starts, stops, and never renders transcript text", async () => {
    await act(async () => root.render(<MicrophoneButton />));
    await flushEffects();
    const button = container.querySelector("button")!;

    await act(async () => button.click());
    expect(ipcMocks.startVoiceInput).toHaveBeenCalledOnce();
    expect(button.getAttribute("aria-label")).toBe("结束录音");
    expect(container.textContent).toContain("录音中");

    await act(async () => button.click());
    expect(ipcMocks.stopVoiceInput).toHaveBeenCalledOnce();
    expect(button.disabled).toBe(true);
    expect(container.textContent).toContain("正在识别");

    act(() => ipcMocks.handlers[0](state("submitted", 3)));
    expect(container.textContent).toContain("已发送");
    expect(container.textContent).not.toContain("private transcript");
  });

  it("cancels only an active recording on Escape", async () => {
    await act(async () => root.render(<MicrophoneButton />));
    await flushEffects();
    const button = container.querySelector("button")!;
    await act(async () => button.click());

    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
      await Promise.resolve();
    });
    expect(ipcMocks.cancelVoiceInput).toHaveBeenCalledOnce();
    expect(container.textContent).toContain("已取消");

    await act(async () => {
      window.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape" }));
      await Promise.resolve();
    });
    expect(ipcMocks.cancelVoiceInput).toHaveBeenCalledOnce();
  });

  it("ignores stale states and shows the fixed no-speech prompt", async () => {
    await act(async () => root.render(<MicrophoneButton />));
    await flushEffects();

    act(() => ipcMocks.handlers[0](state("notHeard", 4)));
    act(() => ipcMocks.handlers[0](state("recording", 3)));
    expect(container.textContent).toContain("没听清，再说一次？");
    expect(container.textContent).not.toContain("录音中");
  });

  it("unregisters the backend event listener", async () => {
    await act(async () => root.render(<MicrophoneButton />));
    await flushEffects();
    await act(async () => root.unmount());
    root = createRoot(container);
    expect(ipcMocks.unlisten).toHaveBeenCalledOnce();
  });
});
