import { useEffect, useState, type CSSProperties } from "react";

import {
  getPetAssetUrl,
  loadPetManifest,
  type PetManifest,
  type PetVisualState,
} from "../lib/pet-assets";

interface PetSpriteProps {
  state: PetVisualState;
  animated?: boolean;
  replayKey?: number | string;
  className?: string;
}

export function PetSprite({
  state,
  animated = false,
  replayKey = 0,
  className = "",
}: PetSpriteProps) {
  const [manifest, setManifest] = useState<PetManifest>();
  const [error, setError] = useState<string>();

  useEffect(() => {
    let active = true;
    loadPetManifest()
      .then((value) => {
        if (active) setManifest(value);
      })
      .catch((reason: unknown) => {
        if (active) {
          setError(reason instanceof Error ? reason.message : "桌宠资源不可用。");
        }
      });
    return () => {
      active = false;
    };
  }, []);

  if (error) {
    return (
      <div className={`pet-sprite pet-sprite--error ${className}`} role="status">
        {error}
      </div>
    );
  }
  if (!manifest) {
    return (
      <div
        className={`pet-sprite pet-sprite--loading ${className}`}
        aria-label="正在加载桌宠"
      />
    );
  }

  const asset = manifest.states[state];
  const filename = animated ? asset.animation : asset.still;
  const style = {
    "--pet-source-width": manifest.container.width,
    "--pet-source-height": manifest.container.height,
  } as CSSProperties;

  return (
    <div className={`pet-sprite ${className}`} style={style}>
      <img
        key={`${state}-${animated ? "motion" : "still"}-${replayKey}`}
        src={getPetAssetUrl(filename)}
        alt="AI Jarvis 桌宠"
        draggable={false}
      />
    </div>
  );
}
