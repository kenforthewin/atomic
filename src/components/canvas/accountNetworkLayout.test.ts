import { describe, expect, it } from 'vitest';
import {
  accountLayoutStorageKey,
  computeAccountChartLayout,
  emptyAccountLayoutSettings,
  readAccountLayoutSettings,
  writeAccountLayoutSettings,
  type AccountLayoutNode,
} from './accountNetworkLayout';

const nodes: AccountLayoutNode[] = [
  { id: 'account', title: 'Peachtree', category: 'account' },
  { id: 'greg', title: 'Greg', category: 'executive' },
  { id: 'jatin', title: 'Jatin', category: 'executive' },
  { id: 'school', title: 'UT Austin', category: 'alumni' },
  { id: 'door', title: 'Schools and alumni', category: 'alumni' },
  { id: 'orphan', title: 'Unlinked note', category: 'board' },
];

const links = [
  { source: 'account', target: 'greg' },
  { source: 'account', target: 'jatin' },
  { source: 'account', target: 'door' },
  { source: 'greg', target: 'school' },
  { source: 'door', target: 'school' }, // shared child stays one node
  { source: 'school', target: 'greg' }, // cycle does not change the root depth
];

describe('account chart layout', () => {
  it('places the account first, includes every node once, and keeps unlinked notes separate', () => {
    const layout = computeAccountChartLayout(nodes, links, 'account', 'top-down');
    expect(Object.keys(layout).sort()).toEqual(nodes.map(node => node.id).sort());
    expect(layout.account.y).toBe(0);
    expect(layout.greg.y).toBe(-170);
    expect(layout.jatin.y).toBe(-170);
    expect(layout.school.y).toBe(-340);
    expect(layout.orphan.y).toBe(-510);
  });

  it('uses the same stable ordering in the horizontal orientation', () => {
    const top = computeAccountChartLayout(nodes, links, 'account', 'top-down');
    const horizontal = computeAccountChartLayout([...nodes].reverse(), [...links].reverse(), 'account', 'left-to-right');
    expect(horizontal.account.x).toBe(0);
    expect(horizontal.greg.x).toBe(260);
    expect(horizontal.school.x).toBe(520);
    expect(horizontal.orphan.x).toBe(780);
    expect(Math.sign(top.greg.x - top.jatin.x)).toBe(Math.sign(horizontal.greg.y - horizontal.jatin.y));
  });

  it('wraps wide levels into readable bands without changing the link graph', () => {
    const many = Array.from({ length: 11 }, (_, index): AccountLayoutNode => ({
      id: `door-${index}`, title: `Door ${index}`, category: 'network',
    }));
    const positions = computeAccountChartLayout(
      [nodes[0], ...many],
      many.map(node => ({ source: 'account', target: node.id })),
      'account', 'top-down',
    );
    expect(new Set(many.map(node => positions[node.id].y))).toEqual(new Set([-170, -340, -510]));
    expect(Math.max(...many.map(node => Math.abs(positions[node.id].x)))).toBe(440);
  });

  it('does not invent a layout when the account root is missing', () => {
    expect(computeAccountChartLayout(nodes, links, 'missing', 'top-down')).toEqual({});
  });
});

describe('device layout settings', () => {
  const values = new Map<string, string>();
  const storage = {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => { values.set(key, value); },
  };

  it('keeps each account and database separate and restores chart positions', () => {
    values.clear();
    const settings = emptyAccountLayoutSettings();
    settings.mode = 'left-to-right';
    settings.positions['left-to-right'].greg = { x: 42, y: 12 };
    expect(writeAccountLayoutSettings(storage, 'db-a', 'account-a', settings)).toBe(true);
    expect(readAccountLayoutSettings(storage, 'db-a', 'account-a')).toEqual(settings);
    expect(readAccountLayoutSettings(storage, 'db-b', 'account-a')).toEqual(emptyAccountLayoutSettings());
    expect(readAccountLayoutSettings(storage, 'db-a', 'account-b')).toEqual(emptyAccountLayoutSettings());
    expect(accountLayoutStorageKey('db-a', 'account-a')).not.toBe(accountLayoutStorageKey('db-a', 'account-b'));
  });

  it('starts account networks with recorded links and preserves an explicit freeform choice', () => {
    values.clear();
    expect(readAccountLayoutSettings(storage, 'db', 'account').mode).toBe('top-down');
    const settings = emptyAccountLayoutSettings();
    settings.mode = 'freeform';
    writeAccountLayoutSettings(storage, 'db', 'account', settings);
    expect(readAccountLayoutSettings(storage, 'db', 'account').mode).toBe('freeform');
  });

  it('ignores corrupt and nonfinite saved coordinates', () => {
    values.clear();
    storage.setItem(accountLayoutStorageKey('db', 'account'), JSON.stringify({
      mode: 'top-down',
      positions: { 'top-down': { valid: { x: 1, y: 2 }, bad: { x: 'NaN', y: 4 } } },
    }));
    const loaded = readAccountLayoutSettings(storage, 'db', 'account');
    expect(loaded.positions['top-down']).toEqual({ valid: { x: 1, y: 2 } });
    storage.setItem(accountLayoutStorageKey('db', 'account'), '{broken');
    expect(readAccountLayoutSettings(storage, 'db', 'account')).toEqual(emptyAccountLayoutSettings());
  });
});
