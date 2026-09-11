import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type {
  ChatStatusEvent,
  ClearTransientOutputEvent,
  ObservationResultEvent,
} from "../lib/types";

const ipcMocks = vi.hoisted(() => ({
  chatHandlers: [] as Array<(event: unknown) => void>,
  clearHandlers: [] as Array<(event: unknown) => void>,
  observationHandlers: [] as Array<(event: unknown) => void>,
  chatUnlisten: vi.fn(),
  clearChatHistory: vi.fn(),
  clearUnlisten: vi.fn(),
  observationUnlisten: vi.fn(),
  getChatHistory: vi.fn(),
  sendTextMessage: vi.fn(),
}));

vi.mock("../lib/ipc", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/ipc")>()),
  clearChatHistory: ipcMocks.clearChatHistory,
  getChatHistory: ipcMocks.getChatHistory,
  sendTextMessage: ipcMocks.sendTextMessage,
  listenChatStatus: vi.fn(async (handler: (event: unknown) => void) => {
    ipcMocks.chatHandlers.push(handler);
    return ipcMocks.chatUnlisten;
  }),
  listenClearTransientOutput: vi.fn(
    async (handler: (event: unknown) => void) => {
      ipcMocks.clearHandlers.push(handler);
      return ipcMocks.clearUnlisten;
    },
  ),
  listenObservationResult: vi.fn(async (handler: (event: unknown) => void) => {
    ipcMocks.observationHandlers.push(handler);
    return ipcMocks.observationUnlisten;
  }),
}));

import { SpeechBubble } from "./SpeechBubble";
import { useChatSession } from "../hooks/useChatSession";

function ChatHarness({ generation, revision = 1 }: { generation: number; revision?: number }) {
  const chat = useChatSession(generation, revision);
  return (
    <div data-ready={String(chat.ready)}>
      <button type="button" onClick={() => void chat.send("hello")}>send</button>
      <button type="button" onClick={() => void chat.clear()}>clear</button>
      {chat.exchanges.map((exchange) => (
        <SpeechBubble key={exchange.key} exchange={exchange} />
      ))}
      {chat.error ? <p role="alert">{chat.error}</p> : null}
    </div>
  );
}

async function flushEffects() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

function emitChat(event: ChatStatusEvent) {
  for (const handler of ipcMocks.chatHandlers) handler(event);
}

function emitClear(event: ClearTransientOutputEvent) {
  for (const handler of ipcMocks.clearHandlers) handler(event);
}

function emitObservation(event: ObservationResultEvent) {
  for (const handler of ipcMocks.observationHandlers) handler(event);
}

describe("AG-04 chat UI", () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (
      globalThis as typeof globalThis & {
        IS_REACT_ACT_ENVIRONMENT: boolean;
      }
    ).IS_REACT_ACT_ENVIRONMENT = true;
    ipcMocks.chatHandlers.length = 0;
    ipcMocks.clearHandlers.length = 0;
    ipcMocks.observationHandlers.length = 0;
    ipcMocks.chatUnlisten.mockReset();
    ipcMocks.clearUnlisten.mockReset();
    ipcMocks.observationUnlisten.mockReset();
    ipcMocks.clearChatHistory.mockReset().mockResolvedValue(undefined);
    ipcMocks.sendTextMessage.mockReset().mockResolvedValue({
      requestId: 1,
      status: "accepted",
    });
    ipcMocks.getChatHistory.mockReset().mockResolvedValue([
      {
        user: "earlier question",
        assistant: "earlier answer",
        userTokens: 2,
        assistantTokens: 3,
      },
    ]);
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
  });

  it("hydrates history and renders every explicit request state", async () => {
    await act(async () => root.render(<ChatHarness generation={3} />));
    await flushEffects();
    expect(container.firstElementChild?.getAttribute("data-ready")).toBe("true");
    expect(container.textContent).toContain("earlier answer");

    act(() =>
      emitChat({
        requestId: 7,
        runtimeGeneration: 3,
        status: "accepted",
        userMessage: "new question",
        reply: null,
        failure: null,
      }),
    );
    expect(container.textContent).toContain("正在思考");

    act(() =>
      emitChat({
        requestId: 7,
        runtimeGeneration: 3,
        status: "replied",
        userMessage: "new question",
        reply: "new answer",
        failure: null,
      }),
    );
    expect(container.textContent).toContain("new answer");

    for (const [requestId, failure, copy] of [
      [8, "modelUnavailable", "本地模型尚未就绪"],
      [9, "contextUnavailable", "暂未获取到屏幕画面"],
      [10, "timeout", "本轮回复超时"],
      [11, "invalidResponse", "模型回复格式无效"],
    ] as const) {
      act(() =>
        emitChat({
          requestId,
          runtimeGeneration: 3,
          status: "failed",
          userMessage: "question",
          reply: null,
          failure,
        }),
      );
      expect(container.textContent).toContain(copy);
    }

    act(() =>
      emitChat({
        requestId: 12,
        runtimeGeneration: 3,
        status: "cancelled",
        userMessage: "cancel me",
        reply: null,
        failure: null,
      }),
    );
    expect(container.textContent).toContain("本轮已取消");
  });

  it("keeps direct chat across privacy generations while dropping stale events", async () => {
    await act(async () => root.render(<ChatHarness generation={3} />));
    await flushEffects();

    act(() =>
      emitClear({
        revision: 2,
        runtimeGeneration: 3,
        reason: "proactiveResponseDisabled",
      }),
    );
    expect(container.textContent).toContain("earlier answer");

    act(() =>
      emitClear({
        revision: 3,
        runtimeGeneration: 4,
        reason: "privacyPaused",
      }),
    );
    expect(container.textContent).toContain("earlier answer");

    await act(async () => root.render(<ChatHarness generation={4} />));
    expect(container.textContent).toContain("earlier answer");

    act(() =>
      emitChat({
        requestId: 12,
        runtimeGeneration: 3,
        status: "replied",
        userMessage: "stale question",
        reply: "stale answer",
        failure: null,
      }),
    );
    expect(container.textContent).not.toContain("stale answer");
  });

  it("shows only current proactive responses and clears them on source changes", async () => {
    await act(async () => root.render(<ChatHarness generation={3} revision={4} />));
    await flushEffects();

    act(() =>
      emitObservation({
        runtimeRevision: 4,
        runtimeGeneration: 3,
        latestFrameSequence: 8,
        scene: "other",
        needResponse: false,
        nextCheckSeconds: 10,
        message: "",
      }),
    );
    expect(container.textContent).not.toContain("主动提醒");

    act(() =>
      emitObservation({
        runtimeRevision: 4,
        runtimeGeneration: 3,
        latestFrameSequence: 9,
        scene: "other",
        needResponse: true,
        nextCheckSeconds: 10,
        message: "主动提醒",
      }),
    );
    expect(container.textContent).toContain("主动提醒");

    act(() =>
      emitClear({
        revision: 5,
        runtimeGeneration: 3,
        reason: "screenDisabled",
      }),
    );
    expect(container.textContent).not.toContain("主动提醒");
    expect(container.textContent).toContain("earlier answer");

    act(() =>
      emitObservation({
        runtimeRevision: 4,
        runtimeGeneration: 3,
        latestFrameSequence: 10,
        scene: "other",
        needResponse: true,
        nextCheckSeconds: 10,
        message: "延迟旧提醒",
      }),
    );
    expect(container.textContent).not.toContain("延迟旧提醒");
  });

  it("treats a game-looking screen as a bubble response while a course session is active", async () => {
    await act(async () => root.render(<ChatHarness generation={3} revision={4} />));
    await flushEffects();

    act(() =>
      emitObservation({
        runtimeRevision: 4,
        runtimeGeneration: 3,
        latestFrameSequence: 11,
        scene: "game",
        activeSession: "course",
        needResponse: true,
        nextCheckSeconds: 90,
        message: "先回到老师刚才讲的条件。",
      }),
    );
    expect(container.textContent).toContain("先回到老师刚才讲的条件");

    act(() =>
      emitObservation({
        runtimeRevision: 4,
        runtimeGeneration: 3,
        latestFrameSequence: 12,
        scene: "game",
        activeSession: "none",
        needResponse: true,
        nextCheckSeconds: 3,
        message: "普通游戏弹幕",
      }),
    );
    expect(container.textContent).not.toContain("普通游戏弹幕");
  });

  it("submits, clears, and unregisters every event listener", async () => {
    await act(async () => root.render(<ChatHarness generation={3} />));
    await flushEffects();

    const [send, clear] = container.querySelectorAll("button");
    await act(async () => send.click());
    expect(ipcMocks.sendTextMessage).toHaveBeenCalledWith("hello");
    await act(async () => clear.click());
    expect(ipcMocks.clearChatHistory).toHaveBeenCalledOnce();

    await act(async () => root.unmount());
    root = createRoot(container);
    expect(ipcMocks.chatUnlisten).toHaveBeenCalledOnce();
    expect(ipcMocks.clearUnlisten).toHaveBeenCalledOnce();
    expect(ipcMocks.observationUnlisten).toHaveBeenCalledOnce();
  });
});
