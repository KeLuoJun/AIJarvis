export type PetVisualState = "idle" | "normal" | "paused";

export interface PetStateAsset {
  animation: string;
  still: string;
}

export interface PetManifest {
  schemaVersion: 1;
  container: {
    width: number;
    height: number;
  };
  states: Record<PetVisualState, PetStateAsset>;
}

const expectedAssets: Record<PetVisualState, PetStateAsset> = {
  idle: { animation: "idle.gif", still: "idle.png" },
  normal: { animation: "normal.gif", still: "normal.png" },
  paused: { animation: "closed-eyes.gif", still: "closed-eyes.png" },
};
const stateNames = Object.keys(expectedAssets) as PetVisualState[];
const safeAssetName = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

export function parsePetManifest(value: unknown): PetManifest {
  if (!isRecord(value) || value.schemaVersion !== 1) {
    throw new Error("桌宠资源清单版本无效。");
  }
  if (!isRecord(value.container)) {
    throw new Error("桌宠资源清单缺少容器尺寸。");
  }
  const width = value.container.width;
  const height = value.container.height;
  if (
    typeof width !== "number" ||
    typeof height !== "number" ||
    width !== 320 ||
    height !== 480
  ) {
    throw new Error("桌宠容器尺寸无效。");
  }
  if (!isRecord(value.states)) {
    throw new Error("桌宠资源清单缺少状态。");
  }

  const states = {} as Record<PetVisualState, PetStateAsset>;
  for (const state of stateNames) {
    const asset = value.states[state];
    if (!isRecord(asset)) {
      throw new Error(`桌宠资源清单缺少 ${state} 状态。`);
    }
    const animation = asset.animation;
    const still = asset.still;
    if (
      typeof animation !== "string" ||
      typeof still !== "string" ||
      !safeAssetName.test(animation) ||
      !safeAssetName.test(still) ||
      animation !== expectedAssets[state].animation ||
      still !== expectedAssets[state].still
    ) {
      throw new Error(`桌宠 ${state} 状态包含无效资源路径。`);
    }
    states[state] = { animation, still };
  }

  return {
    schemaVersion: 1,
    container: { width, height },
    states,
  };
}

export function getPetAssetUrl(filename: string): string {
  return new URL(`assets/pet/${filename}`, document.baseURI).href;
}

let manifestRequest: Promise<PetManifest> | undefined;

export function loadPetManifest(): Promise<PetManifest> {
  manifestRequest ??= fetch(getPetAssetUrl("manifest.json"))
    .then(async (response) => {
      if (!response.ok) {
        throw new Error(`桌宠资源清单读取失败 (${response.status})。`);
      }
      return parsePetManifest(await response.json());
    })
    .catch((error: unknown) => {
      manifestRequest = undefined;
      throw error;
    });
  return manifestRequest;
}
