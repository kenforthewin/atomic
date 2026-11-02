import type { AtomLink, CanvasAtomPosition, CanvasEdgeData } from '../../lib/api';
import { accountNetworkCategory } from './accountNetworkCategories';

export type AccountNetworkEdge = CanvasEdgeData & {
  relation: 'account-executive' | 'research-door' | 'education' | 'affiliation' | 'door-entity' | 'entry-point';
};

/** Only render recorded account structure and sourced affiliations, never unverified research paths. */
export function accountNetworkEdges(atoms: CanvasAtomPosition[], links: AtomLink[]): AccountNetworkEdge[] {
  const byId = new Map(atoms.map(atom => [atom.atom_id, atom]));
  const seen = new Set<string>();
  const result: AccountNetworkEdge[] = [];

  for (const link of links) {
    if (link.status !== 'resolved' || !link.target_atom_id) continue;
    const source = byId.get(link.source_atom_id);
    const target = byId.get(link.target_atom_id);
    if (!source || !target) continue;
    const sourceCategory = accountNetworkCategory(source.source_url)?.key;
    const targetCategory = accountNetworkCategory(target.source_url)?.key;
    let relation: AccountNetworkEdge['relation'] | null = null;
    let weight = 0;
    if (sourceCategory === 'account' && targetCategory === 'executive') {
      relation = 'account-executive'; weight = 1;
    } else if (sourceCategory === 'account' && target.source_url?.includes('/door/')) {
      relation = 'research-door'; weight = 0.6;
    } else if (sourceCategory === 'executive' && target.source_url?.includes('/school/')) {
      relation = 'education'; weight = 0.8;
    } else if (sourceCategory === 'executive' && target.source_url?.includes('/community/')) {
      relation = 'affiliation'; weight = 0.8;
    } else if (source.source_url?.includes('/door/') && (targetCategory === 'alumni' || targetCategory === 'communities')) {
      relation = 'door-entity'; weight = 0.45;
    }
    if (!relation) continue;
    const key = [source.atom_id, target.atom_id].sort().join(':');
    if (seen.has(key)) continue;
    seen.add(key);
    result.push({ source: source.atom_id, target: target.atom_id, weight, relation });
  }
  return result;
}
