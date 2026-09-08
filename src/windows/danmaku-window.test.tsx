import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type {
  ClearTransientOutputEvent,
  ObservationResultEvent,
} from "../lib/types";

const ipcMocks = vi.hoisted(() => ({
  getDanmakuState: vi.fn(),
  listenClearTransientOutput: vi.fn(),
  listenObservationResult: vi.fn(),
}));

vi.mock("../lib/ipc", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/ipc")>()),
  ...ipcMocks,
}));

import { DanmakuWindow } from "./DanmakuWindow";

function observation(
  sequence: number,
  overrides: Partial<ObservationResultEvent> = {},
): ObservationResultEvent {
  return {
    runtimeRevision: 2,
    runtimeGeneration: 2,
    latestFrameSequence: sequence,
    scene: "game",
    needResponse: true,
    nextCheckSeconds: 5,
    message: "第一条\n第二条\n第三条",
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

describe("AG-07 danmaku window", () => {
  let container: HTMLDivElement;
  let root: Root;
  let emitObservation: (event: ObservationResultEvent) => void;
  let emitClear: (event: ClearTransientOutputEvent) => void;
  let observationUnlisten: ReturnType<typeof vi.fn>;
  let clearUnlisten: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    (
      globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }
    ).IS_REACT_ACT_ENVIRONMENT = true;
    vi.useFakeTimers();
    vi.setSystemTime(0);
    vi.spyOn(Math, "random").mockReturnValue(0);
    Object.defineProperty(window, "matchMedia", {
      configurable: true,
      value: vi.fn().mockReturnValue({ matches: false }),
    });
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
    observationUnlisten = vi.fn();
    clearUnlisten = vi.fn();
    ipcMocks.getDanmakuState.mockReset().mockResolvedValue(null);
    ipcMocks.listenObservationResult.mockReset().mockImplementation(
      async (handler: (event: ObservationResultEvent) => void) => {
        emitObservation = handler;
        return observationUnlisten;
      },
    );
    ipcMocks.listenClearTransientOutput.mockReset().mockImplementation(
      async (handler: (event: ClearTransientOutputEvent) => void) => {
        emitClear = handler;
        return clearUnlisten;
      },
    );
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    vi.restoreAllMocks();
    vi.useRealTimers();
  });

  it("registers listeners before recovering the current game batch", async () => {
    ipcMocks.getDanmakuState.mockResolvedValue(observation(4));
    await act(async () => root.render(<DanmakuWindow />));
    await flushEffects();

    expect(ipcMocks.listenObservationResult).toHaveBeenCalledBefore(
      ipcMocks.getDanmakuState,
    );
    expect(ipcMocks.listenClearTransientOutput).toHaveBeenCalledBefore(
      ipcMocks.getDanmakuState,
    );
    expect(container.querySelector("main")?.dataset.activeCount).toBe("1");
    expect(container.textContent).toContain("第一条");
  });

  it("replaces batches and clears on scene changes, source clears, and resize", async () => {
    await act(async () => root.render(<DanmakuWindow />));
    await flushEffects();

    act(() => emitObservation(observation(1)));
    expect(container.textContent).toContain("第一条");
    act(() => vi.advanceTimersByTime(400));
    expect(container.querySelector("main")?.dataset.activeCount).toBe("2");

    act(() =>
      emitObservation(observation(2, { needResponse: false, message: "" })),
    );
    expect(container.querySelector("main")?.dataset.activeCount).toBe("2");

    act(() =>
      emitObservation(
        observation(3, { message: "新一\n新二\n新三" }),
      ),
    );
    expect(container.textContent).not.toContain("第一条");
    expect(container.textContent).toContain("新一");

    act(() => emitObservation(observation(4, { scene: "other", message: "普通" })));
    expect(container.querySelector("main")?.dataset.activeCount).toBe("0");

    act(() => emitObservation(observation(5)));
    act(() =>
      emitClear({
        revision: 3,
        runtimeGeneration: 3,
        reason: "proactiveResponseDisabled",
      }),
    );
    expect(container.querySelector("main")?.dataset.activeCount).toBe("0");

    act(() => emitObservation(observation(6, { runtimeGeneration: 4 })));
    act(() => window.dispatchEvent(new Event("resize")));
    expect(container.querySelector("main")?.dataset.activeCount).toBe("0");
  });

  it("ignores stale events, suppresses reduced motion, and unregisters listeners", async () => {
    await act(async () => root.render(<DanmakuWindow />));
    await flushEffects();

    act(() => emitObservation(observation(8)));
    act(() => emitObservation(observation(7, { message: "旧一\n旧二\n旧三" })));
    expect(container.textContent).not.toContain("旧一");

    (window.matchMedia as ReturnType<typeof vi.fn>).mockReturnValue({
      matches: true,
    });
    act(() => emitObservation(observation(9)));
    expect(container.querySelector("main")?.dataset.activeCount).toBe("0");

    await act(async () => root.unmount());
    root = createRoot(container);
    expect(observationUnlisten).toHaveBeenCalledOnce();
    expect(clearUnlisten).toHaveBeenCalledOnce();
  });
});
