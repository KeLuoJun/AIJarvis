import { beforeEach, describe, expect, it, vi } from "vitest";

const { invokeMock, listenMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  listenMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/event", () => ({ listen: listenMock }));

import {
  chatStatusEvent,
  cancelVoiceInput,
  clearChatHistory,
  clearTransientOutputEvent,
  closeAppWindow,
  describeIpcError,
  getBootstrapState,
  getChatHistory,
  getDanmakuState,
  getRuntimeState,
  getVoiceInputState,
  listenChatStatus,
  listenBootstrapProgress,
  listenClearTransientOutput,
  listenObservationResult,
  listenRuntimeState,
  listenVoiceInputStatus,
  observationResultEvent,
  quitApp,
  runtimeStateChangedEvent,
  sendTextMessage,
  setFeatureEnabled,
  setPrivacyPaused,
  showAppWindow,
  startVoiceInput,
  stopVoiceInput,
  summonAiJarvis,
  voiceInputStatusEvent,
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

  it("uses the dedicated summon command and progress event", async () => {
    const handler = vi.fn();
    await summonAiJarvis();
    await listenBootstrapProgress(handler);
    expect(invokeMock).toHaveBeenCalledWith("summon_ai_jarvis");
    expect(listenMock).toHaveBeenCalledWith(
      "bootstrap-progress",
      expect.any(Function),
    );
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

  it("uses structured chat commands", async () => {
    await sendTextMessage("hello");
    await getChatHistory();
    await clearChatHistory();

    expect(invokeMock).toHaveBeenNthCalledWith(1, "send_text_message", {
      message: "hello",
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "get_chat_history");
    expect(invokeMock).toHaveBeenNthCalledWith(3, "clear_chat_history");
  });

  it("recovers the current transient danmaku batch without arguments", async () => {
    await getDanmakuState();
    expect(invokeMock).toHaveBeenCalledWith("get_danmaku_state");
  });

  it("uses dedicated voice input commands without transcript arguments", async () => {
    await getVoiceInputState();
    await startVoiceInput();
    await stopVoiceInput();
    await cancelVoiceInput();

    expect(invokeMock).toHaveBeenNthCalledWith(1, "get_voice_input_state");
    expect(invokeMock).toHaveBeenNthCalledWith(2, "start_voice_input");
    expect(invokeMock).toHaveBeenNthCalledWith(3, "stop_voice_input");
    expect(invokeMock).toHaveBeenNthCalledWith(4, "cancel_voice_input");
  });

  it("subscribes only to the fixed backend events", async () => {
    const runtimeHandler = vi.fn();
    const clearHandler = vi.fn();
    const chatHandler = vi.fn();
    const observationHandler = vi.fn();
    const voiceHandler = vi.fn();
    await listenRuntimeState(runtimeHandler);
    await listenClearTransientOutput(clearHandler);
    await listenChatStatus(chatHandler);
    await listenObservationResult(observationHandler);
    await listenVoiceInputStatus(voiceHandler);

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
    expect(listenMock).toHaveBeenNthCalledWith(
      3,
      chatStatusEvent,
      expect.any(Function),
    );
    expect(listenMock).toHaveBeenNthCalledWith(
      4,
      observationResultEvent,
      expect.any(Function),
    );
    expect(listenMock).toHaveBeenNthCalledWith(
      5,
      voiceInputStatusEvent,
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
    expect(describeIpcError({ code: "queueFull", message: "secret" })).toBe(
      "当前消息较多，请稍后再试。",
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
