import type { CameraState, Coordinates, Dimensions } from 'sigma/types';

export function toggleCategory<Key extends string>(selected: ReadonlySet<Key>, key: Key): Set<Key> {
  const next = new Set(selected);
  if (next.has(key)) next.delete(key);
  else next.add(key);
  return next;
}

/** Coordinates are Sigma's normalized graph positions; pixelsPerUnit is measured at camera ratio 1. */
export function categoryCameraTarget(
  points: Coordinates[],
  viewport: Dimensions,
  pixelsPerUnit: number,
): Pick<CameraState, 'x' | 'y' | 'ratio'> | null {
  if (!points.length || viewport.width <= 0 || viewport.height <= 0) return null;
  let minX = Infinity, maxX = -Infinity, minY = Infinity, maxY = -Infinity;
  for (const point of points) {
    minX = Math.min(minX, point.x);
    maxX = Math.max(maxX, point.x);
    minY = Math.min(minY, point.y);
    maxY = Math.max(maxY, point.y);
  }
  const usableWidth = Math.max(1, viewport.width * 0.85 - 96);
  const usableHeight = Math.max(1, viewport.height * 0.85 - 96);
  return {
    x: (minX + maxX) / 2,
    y: (minY + maxY) / 2,
    ratio: points.length === 1 ? 0.35 : Math.max(0.25,
      (maxX - minX) * pixelsPerUnit / usableWidth,
      (maxY - minY) * pixelsPerUnit / usableHeight,
    ),
  };
}
