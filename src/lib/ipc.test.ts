import { beforeEach, describe, expect, it, vi } from "vitest";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

import {
  closeAppWindow,
  describeIpcError,
  getBootstrapState,
  quitApp,
  showAppWindow,
} from "./ipc";

describe("IPC contract", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
  });

  it("uses the registered bootstrap command", async () => {
    await getBootstrapState();
    expect(invokeMock).toHaveBeenCalledWith("get_bootstrap_state");
  });

  it("keeps window kind in a structured argument", async () => {
    await showAppWindow("pet");
    await closeAppWindow("danmaku");
    expect(invokeMock).toHaveBeenNthCalledWith(1, "show_app_window", {
      kind: "pet",
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "close_app_window", {
      kind: "danmaku",
    });
  });

  it("uses the dedicated quit command", async () => {
    await quitApp();
    expect(invokeMock).toHaveBeenCalledWith("quit_app");
  });

  it("normalizes non-error failures", () => {
    expect(describeIpcError("offline")).toBe("offline");
    expect(describeIpcError(null)).toContain("本地服务");
  });
});
