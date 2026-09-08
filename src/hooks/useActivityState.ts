import { useEffect, useState } from "react";

import { getActivityState } from "../lib/ipc";
import type { ActivitySnapshot } from "../lib/types";

export function useActivityState(enabled: boolean) {
  const [activity, setActivity] = useState<ActivitySnapshot>();
  useEffect(() => {
    if (!enabled) { setActivity(undefined); return; }
    let active = true;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      try {
        const next = await getActivityState();
        if (active) setActivity(next);
      } catch {
        if (active) setActivity(undefined);
      }
      if (active) timer = setTimeout(() => void poll(), 2_000);
    };
    void poll();
    return () => { active = false; clearTimeout(timer); };
  }, [enabled]);
  return activity;
}

export const activityCopy: Record<ActivitySnapshot["mode"], string> = {
  available: "正在观察",
  transitioning: "场景切换中",
  focused: "专注陪伴",
  media: "正在跟随内容",
  gaming: "游戏陪伴",
  quiet: "安静待命",
  protected: "感知已避让",
  interacting: "正在与你交流",
};
