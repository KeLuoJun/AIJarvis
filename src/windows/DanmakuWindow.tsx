import { useEffect, useState, type CSSProperties } from "react";

import {
  DanmakuController,
  parseGameDanmaku,
  type DanmakuItem,
} from "../lib/danmaku";
import {
  getDanmakuState,
  listenClearTransientOutput,
  listenObservationResult,
} from "../lib/ipc";
import type {
  ClearTransientOutputEvent,
  ObservationResultEvent,
} from "../lib/types";

export function DanmakuWindow() {
  const [items, setItems] = useState<DanmakuItem[]>([]);

  useEffect(() => {
    let active = true;
    let latestGeneration = -1;
    let latestRevision = -1;
    let latestSequence = -1;
    const controller = new DanmakuController((next) => {
      if (active) setItems(next);
    });

    const isNewObservation = (event: ObservationResultEvent) =>
      event.runtimeGeneration > latestGeneration ||
      (event.runtimeGeneration === latestGeneration &&
        (event.runtimeRevision > latestRevision ||
          (event.runtimeRevision === latestRevision &&
            event.latestFrameSequence > latestSequence)));

    const applyObservation = (event: ObservationResultEvent) => {
      if (!active || !isNewObservation(event)) return;
      latestGeneration = event.runtimeGeneration;
      latestRevision = event.runtimeRevision;
      latestSequence = event.latestFrameSequence;
      if (event.scene !== "game") {
        controller.cancel();
        return;
      }
      if (!event.needResponse) return;
      if (event.message === "开始游戏") {
        controller.startGameEntry({
          width: window.innerWidth,
          height: window.innerHeight,
        });
        return;
      }
      const messages = parseGameDanmaku(event.message);
      if (!messages) {
        controller.cancel();
        return;
      }
      controller.startBatch(messages, {
        width: window.innerWidth,
        height: window.innerHeight,
      });
    };

    const applyClear = (event: ClearTransientOutputEvent) => {
      if (
        !active ||
        event.runtimeGeneration < latestGeneration ||
        (event.runtimeGeneration === latestGeneration &&
          event.revision < latestRevision)
      ) {
        return;
      }
      latestGeneration = event.runtimeGeneration;
      latestRevision = event.revision;
      latestSequence = Number.MAX_SAFE_INTEGER;
      controller.cancel();
    };

    const onResize = () => controller.cancel();
    window.addEventListener("resize", onResize);
    const connect = async () => {
      const unlisteners = await Promise.all([
        listenObservationResult(applyObservation),
        listenClearTransientOutput(applyClear),
      ]);
      if (!active) {
        for (const unlisten of unlisteners) unlisten();
        return [];
      }
      let current: ObservationResultEvent | null = null;
      try {
        current = await getDanmakuState();
      } catch {
        controller.cancel();
      }
      if (!active) {
        for (const unlisten of unlisteners) unlisten();
        return [];
      }
      if (current) applyObservation(current);
      return unlisteners;
    };

    const connection = connect().catch(() => []);
    return () => {
      active = false;
      window.removeEventListener("resize", onResize);
      controller.cancel();
      void connection.then((unlisteners) => {
        for (const unlisten of unlisteners ?? []) unlisten();
      });
    };
  }, []);

  return (
    <main
      className="danmaku-window"
      data-window="danmaku"
      data-active-count={items.length}
      aria-live="off"
    >
      <div className="danmaku-stage">
        {items.map((item) => (
          <div
            className="danmaku-item"
            key={item.id}
            style={
              {
                "--danmaku-top": `${item.top}px`,
                "--danmaku-width": `${item.width}px`,
                "--danmaku-duration": `${item.durationMs}ms`,
              } as CSSProperties
            }
          >
            <span className="danmaku-brand">AIJarvis</span>
            {item.text}
          </div>
        ))}
      </div>
    </main>
  );
}
