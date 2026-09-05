import { afterEach, describe, expect, it, vi } from "vitest";

import {
  DanmakuController,
  danmakuLimits,
  estimateDanmakuWidth,
  parseGameDanmaku,
  type DanmakuItem,
} from "./danmaku";

afterEach(() => vi.useRealTimers());

describe("game message parsing", () => {
  it("normalizes lines, caps overflow, and uses Chinese sentence fallback", () => {
    expect(parseGameDanmaku(" 一 \r\n二\n\n三 ")).toEqual(["一", "二", "三"]);
    expect(parseGameDanmaku("一\n二\n三\n四\n五\n六")).toEqual([
      "一",
      "二",
      "三",
      "四",
      "五",
    ]);
    expect(parseGameDanmaku("漂亮！差一点？稳住。")).toEqual([
      "漂亮！",
      "差一点？",
      "稳住。",
    ]);
  });

  it("rejects an undersized or oversized batch as a whole", () => {
    expect(parseGameDanmaku("一\n二")).toBeNull();
    expect(
      parseGameDanmaku(`${"长".repeat(danmakuLimits.messageMaxChars + 1)}\n二\n三`),
    ).toBeNull();
    expect(estimateDanmakuWidth("中文 AB")).toBeGreaterThan(28);
  });
});

describe("bounded danmaku scheduling", () => {
  function controller(
    changes: DanmakuItem[][],
    options: { maxTrackWaitMs?: number } = {},
  ) {
    return new DanmakuController(
      (items) => changes.push(items),
      {
        now: () => Date.now(),
        setTimeout,
        clearTimeout,
        random: () => 0,
      },
      options,
    );
  }

  it("launches immediately, rotates fixed tracks, and never overlaps entry", () => {
    vi.useFakeTimers();
    vi.setSystemTime(0);
    const changes: DanmakuItem[][] = [];
    const scheduler = controller(changes);
    scheduler.startBatch(["第一条", "第二条", "第三条"], {
      width: 1_280,
      height: 720,
    });

    expect(scheduler.snapshot()).toHaveLength(1);
    expect(scheduler.snapshot()[0].track).toBe(0);
    vi.advanceTimersByTime(400);
    expect(scheduler.snapshot()).toHaveLength(2);
    expect(scheduler.snapshot()[1].track).toBe(1);
    vi.advanceTimersByTime(400);
    expect(scheduler.snapshot()).toHaveLength(3);
    expect(scheduler.snapshot()[2].track).toBe(2);
    expect(changes.at(-1)?.every((item) => item.top < 720 * 0.3)).toBe(true);
  });

  it("waits only for a bounded period when the sole track is unsafe", () => {
    vi.useFakeTimers();
    vi.setSystemTime(0);
    const changes: DanmakuItem[][] = [];
    const scheduler = controller(changes, { maxTrackWaitMs: 160 });
    scheduler.startBatch(["第一条", "第二条", "第三条"], {
      width: 320,
      height: 120,
    });

    vi.advanceTimersByTime(1_200);
    expect(scheduler.snapshot()).toHaveLength(1);
    expect(scheduler.stats().pending).toBe(0);
    expect(scheduler.stats().timers).toBe(1);
  });

  it("keeps 1,000 replacement batches bounded and cancel returns to zero", () => {
    vi.useFakeTimers();
    vi.setSystemTime(0);
    const changes: DanmakuItem[][] = [];
    const scheduler = controller(changes);
    for (let batch = 0; batch < 1_000; batch += 1) {
      scheduler.startBatch(["一", "二", "三", "四", "五"], {
        width: 1_280,
        height: 720,
      });
      expect(scheduler.stats().active).toBeLessThanOrEqual(1);
      expect(scheduler.stats().pending).toBeLessThanOrEqual(4);
      expect(scheduler.stats().timers).toBeLessThanOrEqual(5);
    }

    scheduler.cancel();
    expect(scheduler.stats()).toEqual({ active: 0, pending: 0, timers: 0 });
    vi.runAllTimers();
    expect(scheduler.snapshot()).toEqual([]);
  });
});
