//! Pure helpers and presentation maps for `HealthPanel`.
//!
//! Extracted to keep the main component focused on orchestration (fetch,
//! fix, keyboard, layout). Nothing in this file touches DOM, transport,
//! or React state — it's safe to import into tests.

import type { HealthCheckResult } from '../dashboard/widgets/HealthCheckRow';

// ==================== Report shape ====================

export interface HealthReport {
  overall_score: number;
  overall_status: 'healthy' | 'needs_attention' | 'degraded' | 'unhealthy';
  computed_at: string;
  atom_count: number;
  checks: Record<string, HealthCheckResult>;
  auto_fixable: number;
  requires_review: number;
  previous_score?: number;
  previous_check_scores?: Record<string, number>;
}

export interface FixAction {
  id: string;
  check: string;
  action: string;
  count: number;
  details: string[];
}

export interface FixResponse {
  mode: string;
  actions_taken: FixAction[];
  skipped: Array<{ check: string; reason: string; count: number }>;
  new_score: number;
}

// ==================== Config ====================

export const CHECK_LABELS: Record<string, string> = {
  embedding_coverage: 'Embeddings',
  tagging_coverage: 'Tagging',
  source_uniqueness: 'Source dupes',
  orphan_tags: 'Orphan tags',
  semantic_graph_freshness: 'Semantic graph',
  wiki_coverage: 'Wiki coverage',
  content_quality: 'Content quality',
  tag_health: 'Tag health',
  duplicate_detection: 'Duplicates',
  content_overlap: 'Content overlap',
  contradiction_detection: 'Contradictions',
  boilerplate_pollution: 'Boilerplate',
  broken_internal_links: 'Broken links',
};

// One-line explanation shown under each failing check
export const CHECK_DESCRIPTIONS: Record<string, (data: Record<string, unknown>) => string> = {
  embedding_coverage: (d) => {
    const failed = d.failed as number ?? 0;
    const pending = d.pending as number ?? 0;
    if (failed > 0) return `${failed} atom${failed !== 1 ? 's' : ''} failed to embed — semantic search can't find them`;
    if (pending > 0) return `${pending} atom${pending !== 1 ? 's' : ''} waiting to be embedded`;
    return 'All atoms are embedded';
  },
  tagging_coverage: (d) => {
    const untagged = (d.untagged_complete as number ?? 0) + (d.skipped_untagged as number ?? 0);
    const failed = d.failed as number ?? 0;
    if (untagged > 0) return `${untagged} atom${untagged !== 1 ? 's' : ''} went through the tagger but got zero tags assigned`;
    if (failed > 0) return `${failed} atom${failed !== 1 ? 's' : ''} failed tagging`;
    return 'All atoms are tagged';
  },
  source_uniqueness: (d) => {
    const count = d.count as number ?? 0;
    return `${count} source URL${count !== 1 ? 's' : ''} appear on more than one atom — likely an import bug`;
  },
  orphan_tags: (d) => {
    const count = (d.tags as unknown[])?.length ?? d.count as number ?? 0;
    return `${count} tag${count !== 1 ? 's' : ''} with no atoms and no children — clutter in the tag tree`;
  },
  semantic_graph_freshness: (d) => {
    const n = d.atoms_since_rebuild as number ?? 0;
    return `${n} atom${n !== 1 ? 's' : ''} added or updated since the similarity graph was last built`;
  },
  wiki_coverage: (d) => {
    const missing = d.without_wiki as number ?? 0;
    const stale = d.stale_wikis as number ?? 0;
    const parts = [];
    if (missing > 0) parts.push(`${missing} eligible tag${missing !== 1 ? 's' : ''} have no wiki`);
    if (stale > 0) parts.push(`${stale} wiki${stale !== 1 ? 's' : ''} are out of date`);
    return parts.join(', ');
  },
  content_quality: (d) => {
    const issues = d.issues as Record<string, { count: number }> | undefined;
    if (!issues) return 'Some atoms may need attention';
    const parts = [];
    if (issues.very_short?.count > 0) parts.push(`${issues.very_short.count} too short`);
    if (issues.very_long?.count > 0) parts.push(`${issues.very_long.count} too long`);
    if (issues.no_headings?.count > 0) parts.push(`${issues.no_headings.count} lack headings`);
    if (issues.no_source?.count > 0) parts.push(`${issues.no_source.count} have no source`);
    return parts.join(', ');
  },
  tag_health: (d) => {
    const parts = [];
    if ((d.single_atom_tags as number) > 3) parts.push(`${d.single_atom_tags} single-atom tags`);
    if ((d.rootless_tags as number) > 0) parts.push(`${d.rootless_tags} root-level tags may need nesting`);
    if ((d.similar_name_pairs as number) > 0) parts.push(`${d.similar_name_pairs} similar-name pairs`);
    return parts.join(', ') || 'Tag structure has issues';
  },
  content_overlap: (d) => {
    const overlaps = (d.cross_source_overlaps as number) ?? 0;
    const exact = (d.exact_duplicates as number) ?? 0;
    const templates = (d.template_clones as number) ?? 0;
    const parts = [];
    if (exact > 0) parts.push(`${exact} exact URL duplicate${exact !== 1 ? 's' : ''}`);
    if (templates > 0) parts.push(`${templates} template clone${templates !== 1 ? 's' : ''}`);
    if (overlaps > 0) parts.push(`${overlaps} cross-source overlap${overlaps !== 1 ? 's' : ''} need review`);
    return parts.join(', ') || 'No cross-source content overlap';
  },
  contradiction_detection: (d) => {
    const count = d.potential_contradictions as number ?? 0;
    return `${count} atom pair${count !== 1 ? 's' : ''} on the same topic with differing content`;
  },
  boilerplate_pollution: (d) => {
    const count = d.count as number ?? 0;
    return `${count} atom${count !== 1 ? 's' : ''} have near-identical semantic edges — their embeddings can't be distinguished in search. Usually caused by shared template structure in the content.`;
  },
  broken_internal_links: (d) => {
    const n = (d.broken_count as number) ?? 0;
    const atoms = (d.affected_atoms as number) ?? 0;
    return `${n} link${n !== 1 ? 's' : ''} in ${atoms} atom${atoms !== 1 ? 's' : ''} point to other vault documents but resolve to no atom`;
  },
};

// Human-readable description of each fix_action value
export const FIX_ACTION_LABELS: Record<string, string> = {
  retry_failed_and_process_pending: 'Retry failed embeddings',
  retry_tagging_pipeline: 'Retry failed tagging',
  reset_skipped_untagged_to_pending: 'Re-tag atoms skipped during import',
  delete_orphan_tags: 'Delete unused tags',
  rebuild_semantic_edges: 'Rebuild semantic graph',
  generate_missing_wikis: 'Generate missing wiki articles',
  merge_exact_source_duplicates: 'Merge exact-URL duplicates',
  resolve_internal_links: 'Resolve internal document links to atom URIs',
};

export const STATUS_COLORS = {
  healthy: 'text-green-400',
  needs_attention: 'text-yellow-400',
  degraded: 'text-orange-400',
  unhealthy: 'text-red-400',
};

export const CHECK_ORDER = [
  'embedding_coverage',
  'tagging_coverage',
  'source_uniqueness',
  'orphan_tags',
  'semantic_graph_freshness',
  'wiki_coverage',
  'content_quality',
  'tag_health',
  'content_overlap',
  'contradiction_detection',
  'broken_internal_links',
  'boilerplate_pollution',
];

// ==================== Filter types ====================

export type SeverityFilter = 'all' | 'critical' | 'warning' | 'needs-attention' | 'healthy';
export type FixableFilter = 'all' | 'fixable' | 'manual-only';
export type SortOrder = 'score-asc' | 'score-desc' | 'alphabetical' | 'affected-count';

export interface FilterState {
  severity: SeverityFilter;
  fixable: FixableFilter;
  sort: SortOrder;
}

export const DEFAULT_FILTER: FilterState = {
  severity: 'all',
  fixable: 'all',
  sort: 'score-asc',
};

// ==================== Derived-data helpers ====================

export function pendingActions(report: HealthReport, excluded: Set<string>): { label: string; check: string }[] {
  const actions: { label: string; check: string }[] = [];
  for (const key of CHECK_ORDER) {
    const check = report.checks[key];
    if (!check || check.status === 'ok' || !check.auto_fixable) continue;
    if (excluded.has(key)) continue;
    const label = check.fix_action
      ? (FIX_ACTION_LABELS[check.fix_action] ?? check.fix_action.replace(/_/g, ' '))
      : `Fix ${CHECK_LABELS[key] ?? key}`;
    actions.push({ label, check: key });
  }
  return actions;
}

export function manualOnlyCategories(report: HealthReport): { label: string; check: string; reason?: string }[] {
  const items: { label: string; check: string; reason?: string }[] = [];
  for (const key of CHECK_ORDER) {
    const check = report.checks[key];
    if (!check || check.status === 'ok') continue;
    if (check.auto_fixable) continue;
    items.push({
      label: CHECK_LABELS[key] ?? key,
      check: key,
      reason: check.requires_review ? 'manual review only' : 'no auto-fix available',
    });
  }
  return items;
}

export function extractExamples(check: HealthCheckResult): string[] {
  const d = check.data as Record<string, unknown> | undefined;
  if (!d) return [];
  const out: string[] = [];
  const asEntry = (v: unknown): string | null => {
    if (typeof v === 'string') return v;
    if (v && typeof v === 'object') {
      const o = v as Record<string, unknown>;
      if (typeof o.title === 'string') return o.title;
      if (typeof o.label === 'string') return o.label;
      if (typeof o.name === 'string') return o.name;
      if (typeof o.a === 'string' && typeof o.b === 'string') return `${o.a} ↔ ${o.b}`;
      if (typeof o.atom_a_title === 'string' && typeof o.atom_b_title === 'string') {
        const sim = typeof o.similarity === 'number' ? ` (${Math.round(o.similarity * 100)}% similar)` : '';
        return `${o.atom_a_title} ↔ ${o.atom_b_title}${sim}`;
      }
      if (typeof o.content === 'string') return (o.content as string).slice(0, 80);
    }
    return null;
  };
  const tryList = (v: unknown): void => {
    if (!Array.isArray(v)) return;
    for (const item of v) {
      if (out.length >= 2) break;
      const s = asEntry(item);
      if (s) out.push(s);
    }
  };
  tryList(d.pairs);
  if (out.length < 2) tryList(d.affected_atoms);
  if (out.length < 2) tryList(d.samples);
  if (out.length < 2) tryList(d.items);
  if (out.length < 2) tryList(d.atoms);
  return out;
}

export function extractCount(check: HealthCheckResult): number {
  const d = check.data;
  if (typeof d?.count === 'number') return d.count as number;
  if (Array.isArray(d?.pairs)) return (d.pairs as unknown[]).length;
  if (Array.isArray(d?.affected_atoms)) return (d.affected_atoms as unknown[]).length;
  if (d?.issues) {
    const issues = d.issues as Record<string, { count?: number }>;
    return Object.values(issues).reduce((n, v) => n + (v?.count ?? 0), 0);
  }
  if (typeof d?.rootless_tags === 'number') return d.rootless_tags as number;
  return 0;
}

export function reviewItems(report: HealthReport): { label: string; count: number }[] {
  const items: { label: string; count: number }[] = [];
  for (const key of CHECK_ORDER) {
    const check = report.checks[key];
    if (!check || !check.requires_review) continue;
    const count = extractCount(check);
    if (count === 0) continue;
    items.push({ label: CHECK_LABELS[key] ?? key, count });
  }
  return items;
}

export function getSeverityBadge(score: number): string {
  if (score <= 40) return '🔴';
  if (score <= 70) return '🟠';
  if (score <= 85) return '🟡';
  return '🟢';
}

export function getVisibleChecks(
  report: HealthReport,
  filter: FilterState,
): string[] {
  const visible = CHECK_ORDER.filter(k => {
    const check = report.checks[k];
    if (!check || check.status === 'ok') return false;

    if (filter.severity !== 'all') {
      const score = check.score;
      const sev =
        score <= 40 ? 'critical' :
        score <= 70 ? 'warning' :
        score <= 85 ? 'needs-attention' : 'healthy';
      if (sev !== filter.severity) return false;
    }

    if (filter.fixable === 'fixable' && !check.auto_fixable) return false;
    if (filter.fixable === 'manual-only' && check.auto_fixable) return false;

    return true;
  });

  switch (filter.sort) {
    case 'score-asc':
      visible.sort((a, b) => (report.checks[a]?.score ?? 0) - (report.checks[b]?.score ?? 0));
      break;
    case 'score-desc':
      visible.sort((a, b) => (report.checks[b]?.score ?? 0) - (report.checks[a]?.score ?? 0));
      break;
    case 'alphabetical':
      visible.sort((a, b) => (CHECK_LABELS[a] ?? a).localeCompare(CHECK_LABELS[b] ?? b));
      break;
    case 'affected-count':
      visible.sort((a, b) => {
        const ca = report.checks[a] ? extractCount(report.checks[a]) : 0;
        const cb = report.checks[b] ? extractCount(report.checks[b]) : 0;
        return cb - ca;
      });
      break;
  }

  return visible;
}
