import type { AtomLink, CanvasAtomPosition } from '../../lib/api';
import { accountNetworkCategory, type AccountNetworkCategory } from './accountNetworkCategories';

export interface AccountResearchNote {
  atomId: string;
  content: string;
}

export interface AccountResearchCheck {
  id: string;
  atomId: string;
  title: string;
  kind: 'person' | 'door' | 'other';
  source: string;
  checkedAt: string;
  result: 'data' | 'empty';
  resultLabel: string;
  path: 'verified' | 'none' | 'unknown';
  sourceUrl?: string;
}

export interface VerifiedEntryPoint {
  source: string;
  target: string;
  evidenceUrl: string;
}

/** A dated door placeholder alone does not establish that its category was enriched. */
export function enrichedCategoryKeys(
  atoms: CanvasAtomPosition[],
  checks: AccountResearchCheck[],
): Set<AccountNetworkCategory['key']> {
  const byId = new Map(atoms.map(atom => [atom.atom_id, atom]));
  const enriched = new Set<AccountNetworkCategory['key']>();
  for (const check of checks) {
    const category = accountNetworkCategory(byId.get(check.atomId)?.source_url);
    if (!category) continue;
    enriched.add(category.key);
    const networkCheck = check.id.endsWith(':happenstance:legacy')
      || check.source === 'LinkedIn screenshot'
      || check.source.toLowerCase() === 'happenstance' && /mutual|connection|network/i.test(check.resultLabel);
    if (category.key === 'executive' && networkCheck) {
      enriched.add('network');
    }
  }
  return enriched;
}

/** Absence of a recorded check is distinct from a completed check with an empty result. */
export function nodesWithoutRecordedRun(
  atoms: CanvasAtomPosition[],
  checks: AccountResearchCheck[],
): CanvasAtomPosition[] {
  const checked = new Set(checks.map(check => check.atomId));
  return atoms.filter(atom => !checked.has(atom.atom_id))
    .sort((a, b) => a.title.localeCompare(b.title));
}

const datePattern = '(\\d{4}-\\d{2}-\\d{2})';

function safeHttpUrl(value: string | undefined): string | undefined {
  if (!value) return undefined;
  try {
    const url = new URL(value.replace(/[.,;!?]+$/, ''));
    return url.protocol === 'https:' || url.protocol === 'http:' ? url.href : undefined;
  } catch {
    return undefined;
  }
}

function recordLines(content: string): string[] {
  let inCode = false;
  return content.split('\n').filter(line => {
    if (/^\s*(?:```|~~~)/.test(line)) { inCode = !inCode; return false; }
    return !inCode;
  });
}

/** Only facts explicitly recorded in the account notes appear as completed checks. */
export function accountResearchChecks(
  atoms: CanvasAtomPosition[],
  notes: AccountResearchNote[],
  verified: VerifiedEntryPoint[] = [],
): AccountResearchCheck[] {
  const byId = new Map(atoms.map(atom => [atom.atom_id, atom]));
  const verifiedSources = new Set(verified.map(edge => edge.source));
  const checks: AccountResearchCheck[] = [];

  for (const note of notes) {
    const atom = byId.get(note.atomId);
    if (!atom) continue;
    const category = accountNetworkCategory(atom.source_url);
    if (!category) continue;
    const kind = category.key === 'executive' ? 'person' : atom.source_url?.includes('/door/') ? 'door' : 'other';
    const providers = new Set<string>();
    recordLines(note.content).forEach((line, index) => {
      const record = line.match(/^\s*[-*]\s*Enrichment run:\s*(.+)$/i);
      if (!record) return;
      const [source, checkedAt, outcome, resultLabel, rawSourceUrl] = record[1].split(/\s+\|\s+/);
      if (!source || !/^\d{4}-\d{2}-\d{2}$/.test(checkedAt ?? '')
        || outcome !== 'data' && outcome !== 'empty' || !resultLabel) return;
      providers.add(source.toLowerCase());
      checks.push({
        id: `${note.atomId}:run:${index}`,
        atomId: note.atomId,
        title: atom.title,
        kind,
        source,
        checkedAt,
        result: outcome,
        resultLabel,
        path: verifiedSources.has(note.atomId) ? 'verified' : 'unknown',
        sourceUrl: safeHttpUrl(rawSourceUrl),
      });
    });
    if (category?.key === 'executive') {
      const linkedinMutuals = note.content.match(/^- Mutuals: LinkedIn shows (.+?) and (\d+) other mutual connections in Mark's view$/m);
      const linkedinEvidence = note.content.match(/^- LinkedIn mutuals evidence: Mark-provided screenshot; (\d{4}-\d{2}-\d{2}); first-name labels only; introduction path unverified$/m);
      if (linkedinMutuals && linkedinEvidence) {
        checks.push({
          id: `${note.atomId}:linkedin-mutuals`,
          atomId: note.atomId,
          title: atom.title,
          kind: 'person',
          source: 'LinkedIn screenshot',
          checkedAt: linkedinEvidence[1],
          result: 'data',
          resultLabel: `${linkedinMutuals[1]} + ${linkedinMutuals[2]} other mutual connections`,
          path: verifiedSources.has(note.atomId) ? 'verified' : 'unknown',
        });
      }
      const run = note.content.match(new RegExp(`Happenstance network check:\\s*${datePattern}(?:;\\s*source:\\s*(https?:\\/\\/\\S+))?`, 'i'));
      if (!run || providers.has('happenstance')) continue;
      const profile = note.content.match(/Happenstance-matched profile:\s*(https?:\/\/\S+)/i);
      const noPath = /Common ground[^\n]*no direct connection or named mutual found/i.test(note.content);
      checks.push({
        id: `${note.atomId}:happenstance:legacy`,
        atomId: note.atomId,
        title: atom.title,
        kind: 'person',
        source: 'Happenstance',
        checkedAt: run[1],
        result: profile ? 'data' : 'empty',
        resultLabel: profile ? 'Profile found' : 'No exact profile recorded',
        path: verifiedSources.has(note.atomId) ? 'verified' : noPath ? 'none' : 'unknown',
        sourceUrl: safeHttpUrl(run[2]),
      });
    } else if (kind === 'door') {
      const run = note.content.match(new RegExp(`Record type:[^\\n]*research door;\\s*checked:\\s*${datePattern}`, 'i'));
      const count = note.content.match(/Named public entities currently linked:\s*(\d+)/i);
      if (!run || !count || providers.has('public research')) continue;
      const number = Number(count[1]);
      // A generated empty door is a research placeholder, not evidence that an enrichment ran.
      if (number === 0) continue;
      checks.push({
        id: `${note.atomId}:public-research:legacy`,
        atomId: note.atomId,
        title: atom.title,
        kind: 'door',
        source: 'Public research',
        checkedAt: run[1],
        result: 'data',
        resultLabel: `${number} named ${number === 1 ? 'entity' : 'entities'}`,
        path: verifiedSources.has(note.atomId) ? 'verified' : 'unknown',
      });
    }
  }

  const rank = { data: 1, empty: 2 };
  return checks.sort((a, b) =>
    (a.path === 'verified' ? 0 : rank[a.result])
      - (b.path === 'verified' ? 0 : rank[b.result])
      || (a.kind === 'person' ? 0 : 1) - (b.kind === 'person' ? 0 : 1)
      || a.title.localeCompare(b.title),
  );
}

/** A resolved note link and an evidence URL must both support an animated entry path. */
export function verifiedEntryPoints(
  atoms: CanvasAtomPosition[],
  notes: AccountResearchNote[],
  links: AtomLink[],
): VerifiedEntryPoint[] {
  const included = new Set(atoms.map(atom => atom.atom_id));
  const resolved = new Set(links.filter(link => link.status === 'resolved' && link.target_atom_id)
    .map(link => `${link.source_atom_id}:${link.target_atom_id}`));
  const found = new Map<string, VerifiedEntryPoint>();

  for (const note of notes) {
    if (!included.has(note.atomId)) continue;
    for (const line of recordLines(note.content)) {
      const declaration = line.match(/^\s*[-*]\s+Verified entry point:\s*(.+)$/i);
      if (!declaration) continue;
      const target = declaration[1].match(/\[\[([\w-]+)(?:\|[^\]]*)?\]\]/)?.[1];
      const evidence = safeHttpUrl(declaration[1].match(/\]\((https?:\/\/[^\s)]+)\)/)?.[1]);
      if (!target || !included.has(target) || !evidence || !resolved.has(`${note.atomId}:${target}`)) continue;
      found.set(`${note.atomId}:${target}`, { source: note.atomId, target, evidenceUrl: evidence });
    }
  }
  return [...found.values()];
}
