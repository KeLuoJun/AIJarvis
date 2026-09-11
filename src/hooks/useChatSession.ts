import { useCallback, useEffect, useRef, useState } from "react";

import {
  clearChatHistory,
  describeIpcError,
  getChatHistory,
  listenChatStatus,
  listenClearTransientOutput,
  listenObservationResult,
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
  userMessage?: string;
  source: "direct" | "proactive";
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
    source: "direct",
    reply: event.reply ?? undefined,
    status: event.status,
    failure: event.failure ?? undefined,
  };
}

export function useChatSession(
  runtimeGeneration?: number,
  runtimeRevision?: number,
  options: { includeProactive?: boolean } = {},
) {
  const [exchanges, setExchanges] = useState<ChatExchange[]>([]);
  const [ready, setReady] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string>();
  const generationRef = useRef(runtimeGeneration ?? -1);
  const revisionRef = useRef(runtimeRevision ?? -1);

  useEffect(() => {
    if (runtimeGeneration === undefined) return;
    const previous = generationRef.current;
    generationRef.current = Math.max(previous, runtimeGeneration);
  }, [runtimeGeneration]);

  useEffect(() => {
    if (runtimeRevision !== undefined) {
      revisionRef.current = Math.max(revisionRef.current, runtimeRevision);
    }
  }, [runtimeRevision]);

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

        const unlistenObservation = options.includeProactive === false ? undefined : await listenObservationResult((event) => {
          if (
            !active ||
            event.runtimeGeneration < generationRef.current ||
            event.runtimeRevision < revisionRef.current ||
            !event.needResponse ||
            (event.scene === "game" && event.activeSession !== "course") ||
            !event.message.trim()
          ) {
            return;
          }
          setExchanges((current) => [
            ...current,
            {
              key: `observation-${event.runtimeRevision}-${event.latestFrameSequence}`,
              runtimeGeneration: event.runtimeGeneration,
              reply: event.message,
              status: "replied",
              source: "proactive",
            },
          ]);
        });
        if (unlistenObservation && !active) {
          unlistenObservation();
          return;
        }
        if (unlistenObservation) unlisteners.push(unlistenObservation);

        const unlistenClear = await listenClearTransientOutput((event) => {
          if (!active) return;
          generationRef.current = Math.max(
            generationRef.current,
            event.runtimeGeneration,
          );
          revisionRef.current = Math.max(revisionRef.current, event.revision);
          setExchanges((current) =>
            current.filter((exchange) => exchange.source === "direct"),
          );
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
                source: "direct" as const,
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
