import { useCallback, useEffect, useRef, useState } from "react";

import {
  clearChatHistory,
  describeIpcError,
  getChatHistory,
  listenChatStatus,
  listenClearTransientOutput,
  sendTextMessage,
} from "../lib/ipc";
import type {
  ChatFailureKind,
  ChatRequestStatus,
  ChatStatusEvent,
} from "../lib/types";

export interface ChatExchange {
  key: string;
  requestId?: number;
  runtimeGeneration: number;
  userMessage: string;
  reply?: string;
  status: ChatRequestStatus;
  failure?: ChatFailureKind;
}

function fromEvent(event: ChatStatusEvent): ChatExchange {
  return {
    key: `request-${event.requestId}`,
    requestId: event.requestId,
    runtimeGeneration: event.runtimeGeneration,
    userMessage: event.userMessage,
    reply: event.reply ?? undefined,
    status: event.status,
    failure: event.failure ?? undefined,
  };
}

export function useChatSession(runtimeGeneration?: number) {
  const [exchanges, setExchanges] = useState<ChatExchange[]>([]);
  const [ready, setReady] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string>();
  const generationRef = useRef(runtimeGeneration ?? -1);

  useEffect(() => {
    if (runtimeGeneration === undefined) return;
    const previous = generationRef.current;
    generationRef.current = Math.max(previous, runtimeGeneration);
    if (previous >= 0 && runtimeGeneration > previous) {
      setExchanges([]);
    }
  }, [runtimeGeneration]);

  useEffect(() => {
    let active = true;
    const unlisteners: Array<() => void> = [];

    const connect = async () => {
      try {
        const unlistenChat = await listenChatStatus((event) => {
          if (!active || event.runtimeGeneration < generationRef.current) return;
          setExchanges((current) => {
            const next = fromEvent(event);
            const index = current.findIndex(
              (exchange) => exchange.requestId === event.requestId,
            );
            if (index < 0) return [...current, next];
            const updated = current.slice();
            updated[index] = next;
            return updated;
          });
        });
        if (!active) {
          unlistenChat();
          return;
        }
        unlisteners.push(unlistenChat);

        const unlistenClear = await listenClearTransientOutput((event) => {
          if (!active) return;
          generationRef.current = Math.max(
            generationRef.current,
            event.runtimeGeneration,
          );
          if (event.reason === "privacyPaused" || event.reason === "shutdown") {
            setExchanges([]);
          }
        });
        if (!active) {
          unlistenClear();
          return;
        }
        unlisteners.push(unlistenClear);

        const history = await getChatHistory();
        if (!active) return;
        setExchanges((current) =>
          current.length > 0
            ? current
            : history.map((turn, index) => ({
                key: `history-${index}`,
                runtimeGeneration: generationRef.current,
                userMessage: turn.user,
                reply: turn.assistant,
                status: "replied" as const,
              })),
        );
        setReady(true);
      } catch (reason) {
        if (active) setError(describeIpcError(reason));
      }
    };

    void connect();
    return () => {
      active = false;
      for (const unlisten of unlisteners) unlisten();
    };
  }, []);

  const send = useCallback(async (message: string) => {
    setSubmitting(true);
    setError(undefined);
    try {
      await sendTextMessage(message);
      return true;
    } catch (reason) {
      setError(describeIpcError(reason));
      return false;
    } finally {
      setSubmitting(false);
    }
  }, []);

  const clear = useCallback(async () => {
    setError(undefined);
    try {
      await clearChatHistory();
      setExchanges([]);
    } catch (reason) {
      setError(describeIpcError(reason));
    }
  }, []);

  return { exchanges, ready, submitting, error, send, clear };
}
