export interface OverlapPair {
  pair_id: string;
  atom_a: { id: string; title: string; source?: string; created_at?: string };
  atom_b: { id: string; title: string; source?: string; created_at?: string };
  similarity: number;
  shared_tag_count: number;
  available_actions?: string[];
}

export interface AtomPreview {
  id: string;
  title: string;
  created_at?: string;
}

export interface BoilerplateEntry {
  id: string;
  title: string;
  clone_count: number;
}

export interface ContradictionPair {
  pair_id: string;
  atom_a: { id: string; title: string; source?: string; created_at?: string };
  atom_b: { id: string; title: string; source?: string; created_at?: string };
  similarity: number;
  shared_tag_count: number;
}

export interface RootlessTag {
  id: string;
  name: string;
  atom_count: number;
}

export type ItemStatus = 'idle' | 'saving' | 'done' | 'error';

/// Build a stable pair key matching the backend's pair_key helper.
export function pairKey(a: string, b: string): string {
  return a <= b ? `${a}__${b}` : `${b}__${a}`;
}

export function applyFix(
  check: string,
  itemId: string,
  body: Record<string, unknown>,
) {
  // Lazy import to avoid circular
  return import('../../../../lib/transport').then(({ getTransport }) =>
    getTransport().invoke('apply_health_item_fix', {
      check,
      item_id: itemId,
      ...body,
    }),
  );
}
