import { beforeEach, describe, expect, it, vi } from "vitest";

const { invokeMock, listenMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  listenMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/event", () => ({ listen: listenMock }));

import {
  clearTransientOutputEvent,
  closeAppWindow,
  describeIpcError,
  getBootstrapState,
  getRuntimeState,
  listenClearTransientOutput,
  listenRuntimeState,
  quitApp,
  runtimeStateChangedEvent,
  setFeatureEnabled,
  setPrivacyPaused,
  showAppWindow,
} from "./ipc";

describe("IPC contract", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
    listenMock.mockReset();
    listenMock.mockResolvedValue(() => undefined);
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

  it("keeps runtime mutations in structured arguments", async () => {
    await getRuntimeState();
    await setPrivacyPaused(true);
    await setFeatureEnabled("systemAudio", false);

    expect(invokeMock).toHaveBeenNthCalledWith(1, "get_runtime_state");
    expect(invokeMock).toHaveBeenNthCalledWith(2, "set_privacy_paused", {
      paused: true,
    });
    expect(invokeMock).toHaveBeenNthCalledWith(3, "set_feature_enabled", {
      feature: "systemAudio",
      enabled: false,
    });
  });

  it("subscribes only to the two fixed runtime events", async () => {
    const runtimeHandler = vi.fn();
    const clearHandler = vi.fn();
    await listenRuntimeState(runtimeHandler);
    await listenClearTransientOutput(clearHandler);

    expect(listenMock).toHaveBeenNthCalledWith(
      1,
      runtimeStateChangedEvent,
      expect.any(Function),
    );
    expect(listenMock).toHaveBeenNthCalledWith(
      2,
      clearTransientOutputEvent,
      expect.any(Function),
    );
  });

  it("maps stable error codes without exposing backend details", () => {
    expect(
      describeIpcError({
        code: "configWrite",
        message: "C:\\Users\\private\\settings.json contains a secret",
      }),
    ).toBe("无法保存本地配置，请重试。");
    expect(describeIpcError({ code: "privacyPaused", message: "secret" })).toBe(
      "隐私暂停期间无法执行该操作。",
    );
  });

  it("uses generic copy for unknown error shapes", () => {
    const messages = [
      describeIpcError("offline at C:\\Users\\private"),
      describeIpcError(new Error("secret stack detail")),
      describeIpcError({ code: "futureCode", message: "secret detail" }),
      describeIpcError(null),
    ];

    expect(new Set(messages)).toEqual(
      new Set(["本地服务没有返回可用信息。"]),
    );
    expect(messages.join(" ")).not.toContain("private");
    expect(messages.join(" ")).not.toContain("secret");
  });
});
