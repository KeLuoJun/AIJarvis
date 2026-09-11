import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { BootstrapProgress } from "../lib/types";

const ipcMocks = vi.hoisted(() => ({
  listenBootstrapProgress: vi.fn(),
  quitApp: vi.fn(),
  summonAiJarvis: vi.fn(),
}));

vi.mock("../lib/ipc", () => ipcMocks);
vi.mock("../components/PetSprite", () => ({
  PetSprite: () => <div data-pet-sprite />,
}));

import { LauncherWindow } from "./LauncherWindow";

function progress(overrides: Partial<BootstrapProgress> = {}): BootstrapProgress {
  return {
    phase: "downloading",
    step: "qwenModel",
    label: "Qwen 对话模型",
    detail: "从镜像下载",
    artifactIndex: 1,
    artifactCount: 4,
    currentBytes: 500,
    currentTotalBytes: 1_000,
    completedBytes: 500,
    totalBytes: 1_000,
    ...overrides,
  };
}

describe("launcher window", () => {
  let container: HTMLDivElement;
  let root: Root;
  let progressHandler: ((value: BootstrapProgress) => void) | undefined;

  beforeEach(() => {
    (globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    for (const mock of Object.values(ipcMocks)) mock.mockReset();
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
    ipcMocks.listenBootstrapProgress.mockImplementation(async (handler: (value: BootstrapProgress) => void) => {
      progressHandler = handler;
      return vi.fn();
    });
    ipcMocks.summonAiJarvis.mockResolvedValue(undefined);
    ipcMocks.quitApp.mockResolvedValue(undefined);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
  });

  it("starts only after the summon action and renders real byte progress", async () => {
    await act(async () => root.render(<LauncherWindow />));
    await act(async () => Promise.resolve());

    expect(container.textContent).toContain("召唤 AI Jarvis");
    expect(container.textContent).not.toContain("LOCAL COMPANION / READY BAY");
    expect(container.textContent).toContain("模型运行在本地");
    expect(ipcMocks.summonAiJarvis).not.toHaveBeenCalled();

    const summon = Array.from(container.querySelectorAll("button")).find((button) =>
      button.textContent?.includes("召唤 AI Jarvis"),
    );
    if (!summon) throw new Error("missing summon button");
    await act(async () => summon.click());
    expect(ipcMocks.summonAiJarvis).toHaveBeenCalledOnce();

    act(() => progressHandler?.(progress()));
    expect(container.textContent).toContain("Qwen 对话模型");
    expect(container.textContent).toContain("50%");
    expect(container.querySelector('[role="progressbar"]')?.getAttribute("aria-valuenow")).toBe("50");
  });

  it("shows a retry action after a structured startup failure", async () => {
    await act(async () => root.render(<LauncherWindow />));
    await act(async () => Promise.resolve());

    act(() => progressHandler?.(progress({
      phase: "failed",
      step: "failed",
      label: "启动未完成",
      detail: "模型下载连接失败。",
      totalBytes: 0,
    })));
    expect(container.querySelector("main")?.getAttribute("data-phase")).toBe("failed");
    expect(container.textContent).toContain("模型下载连接失败");

    const retry = Array.from(container.querySelectorAll("button")).find((button) =>
      button.textContent?.includes("重新检查"),
    );
    if (!retry) throw new Error("missing retry button");
    await act(async () => retry.click());
    expect(ipcMocks.summonAiJarvis).toHaveBeenCalledOnce();
  });
});
