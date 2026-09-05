import { describe, expect, it } from "vitest";

import { selectWindowKind } from "./window-label";

describe("window label selection", () => {
  it.each(["control", "pet", "danmaku"] as const)(
    "renders the %s window from its Tauri label",
    (label) => {
      expect(selectWindowKind(label, "?window=control")).toBe(label);
    },
  );

  it("supports a browser-only preview query", () => {
    expect(selectWindowKind("main", "?window=pet")).toBe("pet");
  });

  it("fails closed to the control window", () => {
    expect(selectWindowKind("unknown", "?window=unknown")).toBe("control");
  });
});
