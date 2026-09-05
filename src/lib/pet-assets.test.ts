import { describe, expect, it } from "vitest";

import { parsePetManifest } from "./pet-assets";

const validManifest = {
  schemaVersion: 1,
  container: { width: 320, height: 480 },
  states: {
    idle: { animation: "idle.gif", still: "idle.png" },
    normal: { animation: "normal.gif", still: "normal.png" },
    paused: { animation: "closed-eyes.gif", still: "closed-eyes.png" },
  },
};

describe("pet asset manifest", () => {
  it("accepts the fixed v1 manifest", () => {
    expect(parsePetManifest(validManifest)).toEqual(validManifest);
  });

  it("rejects path traversal from public assets", () => {
    expect(() =>
      parsePetManifest({
        ...validManifest,
        states: {
          ...validManifest.states,
          idle: { animation: "../idle.gif", still: "idle.png" },
        },
      }),
    ).toThrow(/无效资源路径/);
  });

  it("rejects partial state maps", () => {
    expect(() =>
      parsePetManifest({
        ...validManifest,
        states: { idle: validManifest.states.idle },
      }),
    ).toThrow(/normal/);
  });

  it("rejects dimensions outside the fixed desktop container", () => {
    expect(() =>
      parsePetManifest({
        ...validManifest,
        container: { width: 321, height: 480 },
      }),
    ).toThrow(/容器尺寸无效/);
  });

  it("rejects a safe but unexpected state asset", () => {
    expect(() =>
      parsePetManifest({
        ...validManifest,
        states: {
          ...validManifest.states,
          normal: { animation: "idle.gif", still: "normal.png" },
        },
      }),
    ).toThrow(/无效资源路径/);
  });
});
