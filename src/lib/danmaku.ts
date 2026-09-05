export const danmakuLimits = {
  messagesMin: 3,
  messagesMax: 5,
  messageMaxChars: 32,
  launchDelayMinMs: 400,
  launchDelayMaxMs: 1_200,
  topRegionRatio: 0.3,
  trackHeightPx: 46,
  trackInsetPx: 18,
  horizontalGapPx: 28,
  retryDelayMs: 80,
  maxTrackWaitMs: 3_000,
} as const;

export interface DanmakuViewport {
  width: number;
  height: number;
}

export interface DanmakuItem {
  id: number;
  text: string;
  track: number;
  top: number;
  width: number;
  durationMs: number;
  launchedAtMs: number;
}

interface ActiveDanmaku extends DanmakuItem {
  speedPxPerSecond: number;
}

interface TimerApi {
  now: () => number;
  setTimeout: (callback: () => void, delayMs: number) => ReturnType<typeof setTimeout>;
  clearTimeout: (handle: ReturnType<typeof setTimeout>) => void;
  random: () => number;
}

export interface DanmakuControllerOptions {
  retryDelayMs?: number;
  maxTrackWaitMs?: number;
}

export interface DanmakuControllerStats {
  active: number;
  pending: number;
  timers: number;
}

const defaultTimerApi: TimerApi = {
  now: () => performance.now(),
  setTimeout: (callback, delayMs) => globalThis.setTimeout(callback, delayMs),
  clearTimeout: (handle) => globalThis.clearTimeout(handle),
  random: () => Math.random(),
};

export function parseGameDanmaku(message: string): string[] | null {
  const lines = message
    .split(/\r?\n/u)
    .map((part) => part.trim())
    .filter(Boolean);
  if (/[\r\n]/u.test(message) && lines.length >= danmakuLimits.messagesMin) {
    return validateMessages(lines);
  }

  const sentences = message
    .match(/[^。？！]+[。？！]?/gu)
    ?.map((part) => part.trim())
    .filter(Boolean) ?? [];
  return validateMessages(sentences);
}

function validateMessages(messages: string[]): string[] | null {
  const bounded = messages.slice(0, danmakuLimits.messagesMax);
  if (
    bounded.length < danmakuLimits.messagesMin ||
    bounded.some(
      (message) => Array.from(message).length > danmakuLimits.messageMaxChars,
    )
  ) {
    return null;
  }
  return bounded;
}

export function estimateDanmakuWidth(text: string): number {
  const units = Array.from(text).reduce((total, character) => {
    if (/\s/u.test(character)) return total + 0.35;
    if (/^[\x20-\x7e]$/u.test(character)) return total + 0.68;
    return total + 1.08;
  }, 0);
  return Math.ceil(28 + units * 24);
}

export class DanmakuController {
  private readonly timer: TimerApi;
  private readonly onChange: (items: DanmakuItem[]) => void;
  private readonly retryDelayMs: number;
  private readonly maxTrackWaitMs: number;
  private readonly launchTimers = new Set<ReturnType<typeof setTimeout>>();
  private readonly expiryTimers = new Set<ReturnType<typeof setTimeout>>();
  private active: ActiveDanmaku[] = [];
  private viewport: DanmakuViewport = { width: 0, height: 0 };
  private batch = 0;
  private nextId = 1;
  private nextTrack = 0;
  private pending = 0;

  constructor(
    onChange: (items: DanmakuItem[]) => void,
    timer: Partial<TimerApi> = {},
    options: DanmakuControllerOptions = {},
  ) {
    this.onChange = onChange;
    this.timer = { ...defaultTimerApi, ...timer };
    this.retryDelayMs = options.retryDelayMs ?? danmakuLimits.retryDelayMs;
    this.maxTrackWaitMs =
      options.maxTrackWaitMs ?? danmakuLimits.maxTrackWaitMs;
  }

  startBatch(messages: readonly string[], viewport: DanmakuViewport): void {
    this.cancel();
    if (
      messages.length < danmakuLimits.messagesMin ||
      messages.length > danmakuLimits.messagesMax ||
      viewport.width <= 0 ||
      viewport.height <= 0
    ) {
      return;
    }

    this.viewport = viewport;
    const batch = this.batch;
    const startedAt = this.timer.now();
    let delay = 0;
    messages.forEach((message, index) => {
      if (index === 0) {
        this.tryLaunch(message, startedAt, batch);
        return;
      }
      delay += this.randomLaunchDelay();
      this.scheduleLaunch(message, startedAt + delay, batch, delay);
    });
  }

  cancel(): void {
    this.batch += 1;
    for (const timer of this.launchTimers) this.timer.clearTimeout(timer);
    for (const timer of this.expiryTimers) this.timer.clearTimeout(timer);
    this.launchTimers.clear();
    this.expiryTimers.clear();
    this.active = [];
    this.pending = 0;
    this.nextTrack = 0;
    this.publish();
  }

  snapshot(): DanmakuItem[] {
    return this.active.map(({ speedPxPerSecond: _, ...item }) => ({ ...item }));
  }

  stats(): DanmakuControllerStats {
    return {
      active: this.active.length,
      pending: this.pending,
      timers: this.launchTimers.size + this.expiryTimers.size,
    };
  }

  private randomLaunchDelay(): number {
    const range =
      danmakuLimits.launchDelayMaxMs - danmakuLimits.launchDelayMinMs + 1;
    const random = Math.min(0.999_999, Math.max(0, this.timer.random()));
    return danmakuLimits.launchDelayMinMs + Math.floor(random * range);
  }

  private scheduleLaunch(
    message: string,
    dueAt: number,
    batch: number,
    delay: number,
  ): void {
    this.pending += 1;
    let handle: ReturnType<typeof setTimeout>;
    handle = this.timer.setTimeout(() => {
      this.launchTimers.delete(handle);
      this.pending -= 1;
      this.tryLaunch(message, dueAt, batch);
    }, delay);
    this.launchTimers.add(handle);
  }

  private tryLaunch(message: string, dueAt: number, batch: number): void {
    if (batch !== this.batch) return;
    const now = this.timer.now();
    this.pruneExpired(now);
    const item = this.createSafeItem(message, now);
    if (!item) {
      if (now - dueAt < this.maxTrackWaitMs) {
        this.scheduleLaunch(message, dueAt, batch, this.retryDelayMs);
      }
      return;
    }

    this.active.push(item);
    this.publish();
    let handle: ReturnType<typeof setTimeout>;
    handle = this.timer.setTimeout(() => {
      this.expiryTimers.delete(handle);
      if (batch !== this.batch) return;
      this.active = this.active.filter((active) => active.id !== item.id);
      this.publish();
    }, item.durationMs);
    this.expiryTimers.add(handle);
  }

  private createSafeItem(message: string, now: number): ActiveDanmaku | null {
    const trackCount = Math.max(
      1,
      Math.floor(
        (this.viewport.height * danmakuLimits.topRegionRatio -
          danmakuLimits.trackInsetPx * 2) /
          danmakuLimits.trackHeightPx,
      ),
    );
    const width = estimateDanmakuWidth(message);
    const speedPxPerSecond = 112 + Math.min(20, width * 0.025);
    const durationMs = Math.ceil(
      ((this.viewport.width + width) / speedPxPerSecond) * 1_000,
    );

    for (let offset = 0; offset < trackCount; offset += 1) {
      const track = (this.nextTrack + offset) % trackCount;
      const previous = this.active
        .filter((item) => item.track === track)
        .sort((left, right) => right.launchedAtMs - left.launchedAtMs)[0];
      if (
        !previous ||
        canFollow(previous, width, speedPxPerSecond, now, this.viewport.width)
      ) {
        this.nextTrack = (track + 1) % trackCount;
        return {
          id: this.nextId++,
          text: message,
          track,
          top:
            danmakuLimits.trackInsetPx + track * danmakuLimits.trackHeightPx,
          width,
          durationMs,
          launchedAtMs: now,
          speedPxPerSecond,
        };
      }
    }
    return null;
  }

  private pruneExpired(now: number): void {
    this.active = this.active.filter(
      (item) => now - item.launchedAtMs < item.durationMs,
    );
  }

  private publish(): void {
    this.onChange(this.snapshot());
  }
}

function canFollow(
  previous: ActiveDanmaku,
  nextWidth: number,
  nextSpeed: number,
  now: number,
  viewportWidth: number,
): boolean {
  const elapsedSeconds = (now - previous.launchedAtMs) / 1_000;
  const previousRight =
    viewportWidth - previous.speedPxPerSecond * elapsedSeconds + previous.width;
  const availableGap =
    viewportWidth - previousRight - danmakuLimits.horizontalGapPx;
  if (availableGap < 0) return false;
  if (nextSpeed <= previous.speedPxPerSecond) return true;

  const catchSeconds = availableGap / (nextSpeed - previous.speedPxPerSecond);
  const previousRemainingSeconds = Math.max(
    0,
    (previous.durationMs - (now - previous.launchedAtMs)) / 1_000,
  );
  const nextLifetimeSeconds =
    (viewportWidth + nextWidth) / nextSpeed;
  return catchSeconds >= Math.min(previousRemainingSeconds, nextLifetimeSeconds);
}
