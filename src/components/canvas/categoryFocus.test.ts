import { describe, expect, it } from 'vitest';
import { categoryCameraTarget, toggleCategory } from './categoryFocus';

describe('account category focus', () => {
  it('toggles multiple categories independently and clears the last one', () => {
    const account = toggleCategory(new Set<string>(), 'account');
    const both = toggleCategory(account, 'investors');
    const investors = toggleCategory(both, 'account');
    const none = toggleCategory(investors, 'investors');
    expect([...account]).toEqual(['account']);
    expect([...both]).toEqual(['account', 'investors']);
    expect([...investors]).toEqual(['investors']);
    expect(none.size).toBe(0);
  });

  it('frames the union of selected nodes with room around the bounds', () => {
    const target = categoryCameraTarget(
      [{ x: 0.2, y: 0.3 }, { x: 0.8, y: 0.7 }],
      { width: 1000, height: 600 },
      800,
    );
    expect(target?.x).toBeCloseTo(0.5);
    expect(target?.y).toBeCloseTo(0.5);
    expect(target?.ratio).toBeCloseTo(320 / 414);
  });

  it('caps zoom for a single node or tight group and skips an empty selection', () => {
    expect(categoryCameraTarget([{ x: 0.4, y: 0.6 }], { width: 1000, height: 600 }, 800))
      .toEqual({ x: 0.4, y: 0.6, ratio: 0.35 });
    expect(categoryCameraTarget(
      [{ x: 0.4, y: 0.6 }, { x: 0.41, y: 0.61 }],
      { width: 1000, height: 600 },
      800,
    )?.ratio).toBe(0.25);
    expect(categoryCameraTarget([], { width: 1000, height: 600 }, 800)).toBeNull();
  });
});
