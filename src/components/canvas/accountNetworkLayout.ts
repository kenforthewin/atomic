import type { AccountNetworkCategory } from './accountNetworkCategories';

export type AccountLayoutMode = 'freeform' | 'top-down' | 'left-to-right';
export type ChartLayoutMode = Exclude<AccountLayoutMode, 'freeform'>;
export type Point = { x: number; y: number };

export interface AccountLayoutNode {
  id: string;
  title: string;
  category: AccountNetworkCategory['key'];
}

export interface AccountLayoutLink {
  source: string;
  target: string;
}

export interface AccountLayoutSettings {
  mode: AccountLayoutMode;
  positions: Record<ChartLayoutMode, Record<string, Point>>;
}

const CATEGORY_ORDER = [
  'account', 'executive', 'network', 'board', 'alumni', 'new_hires',
  'investors', 'agencies', 'software_stack', 'borrowers', 'competitors', 'communities',
] as const;
const CATEGORY_RANK = new Map<string, number>(CATEGORY_ORDER.map((key, index) => [key, index]));
const STORAGE_PREFIX = 'atomic.account-network-layout.v1';

export function emptyAccountLayoutSettings(): AccountLayoutSettings {
  return { mode: 'top-down', positions: { 'top-down': {}, 'left-to-right': {} } };
}

export function accountLayoutStorageKey(dbId: string, accountRootId: string): string {
  return `${STORAGE_PREFIX}:${dbId}:${accountRootId}`;
}

function validPoint(value: unknown): value is Point {
  if (!value || typeof value !== 'object') return false;
  const point = value as Partial<Point>;
  return Number.isFinite(point.x) && Number.isFinite(point.y);
}

export function readAccountLayoutSettings(
  storage: Pick<Storage, 'getItem'>,
  dbId: string,
  accountRootId: string,
): AccountLayoutSettings {
  const empty = emptyAccountLayoutSettings();
  try {
    const raw = storage.getItem(accountLayoutStorageKey(dbId, accountRootId));
    if (!raw) return empty;
    const parsed = JSON.parse(raw) as Partial<AccountLayoutSettings>;
    if (parsed.mode === 'freeform' || parsed.mode === 'top-down' || parsed.mode === 'left-to-right') empty.mode = parsed.mode;
    for (const mode of ['top-down', 'left-to-right'] as const) {
      const positions = parsed.positions?.[mode];
      if (!positions || typeof positions !== 'object') continue;
      for (const [id, point] of Object.entries(positions)) {
        if (validPoint(point)) empty.positions[mode][id] = point;
      }
    }
  } catch {
    // A corrupt or unavailable device preference should not block the canvas.
  }
  return empty;
}

export function writeAccountLayoutSettings(
  storage: Pick<Storage, 'setItem'>,
  dbId: string,
  accountRootId: string,
  settings: AccountLayoutSettings,
): boolean {
  try {
    storage.setItem(accountLayoutStorageKey(dbId, accountRootId), JSON.stringify(settings));
    return true;
  } catch {
    return false;
  }
}

/** A stable layered drawing of recorded links. Cross-links remain edges, not duplicate nodes. */
export function computeAccountChartLayout(
  nodes: AccountLayoutNode[],
  links: AccountLayoutLink[],
  rootId: string,
  mode: ChartLayoutMode,
): Record<string, Point> {
  const byId = new Map(nodes.map(node => [node.id, node]));
  if (!byId.has(rootId)) return {};
  const sortNodes = (a: string, b: string) => {
    const left = byId.get(a)!;
    const right = byId.get(b)!;
    return (CATEGORY_RANK.get(left.category) ?? 99) - (CATEGORY_RANK.get(right.category) ?? 99)
      || left.title.localeCompare(right.title)
      || a.localeCompare(b);
  };
  const outgoing = new Map<string, string[]>();
  for (const link of links) {
    if (!byId.has(link.source) || !byId.has(link.target) || link.source === link.target) continue;
    const children = outgoing.get(link.source) ?? [];
    children.push(link.target);
    outgoing.set(link.source, children);
  }
  for (const children of outgoing.values()) children.sort(sortNodes);

  const depths = new Map<string, number>([[rootId, 0]]);
  const parents = new Map<string, string>();
  const queue = [rootId];
  for (let index = 0; index < queue.length; index++) {
    const parent = queue[index];
    for (const child of outgoing.get(parent) ?? []) {
      if (depths.has(child)) continue;
      depths.set(child, depths.get(parent)! + 1);
      parents.set(child, parent);
      queue.push(child);
    }
  }

  const levels = new Map<number, string[]>();
  for (const [id, depth] of depths) {
    const level = levels.get(depth) ?? [];
    level.push(id);
    levels.set(depth, level);
  }
  for (const [depth, ids] of levels) {
    if (depth === 0) continue;
    ids.sort((a, b) => {
      const parentA = parents.get(a);
      const parentB = parents.get(b);
      const previous = levels.get(depth - 1) ?? [];
      return previous.indexOf(parentA ?? '') - previous.indexOf(parentB ?? '') || sortNodes(a, b);
    });
  }

  const unlinked = nodes.map(node => node.id).filter(id => !depths.has(id)).sort(sortNodes);
  if (unlinked.length) levels.set(Math.max(...levels.keys()) + 1, unlinked);
  const positions: Record<string, Point> = {};
  const crossGap = mode === 'top-down' ? 220 : 104;
  const depthGap = mode === 'top-down' ? 170 : 260;
  const maxPerBand = 5;
  let band = 0;
  for (const [, ids] of [...levels].sort(([a], [b]) => a - b)) {
    ids.forEach((id, index) => {
      const inBand = index % maxPerBand;
      const count = Math.min(maxPerBand, ids.length - Math.floor(index / maxPerBand) * maxPerBand);
      const cross = (inBand - (count - 1) / 2) * crossGap;
      const depth = band + Math.floor(index / maxPerBand);
      positions[id] = mode === 'top-down'
        ? { x: cross, y: depth === 0 ? 0 : -depth * depthGap }
        : { x: depth * depthGap, y: cross };
    });
    band += Math.ceil(ids.length / maxPerBand);
  }
  return positions;
}
