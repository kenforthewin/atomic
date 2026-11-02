import { describe, expect, it } from 'vitest';
import type { AtomLink, CanvasAtomPosition } from '../../lib/api';
import { accountNetworkEdges } from './accountNetworkEdges';

const prefix = 'atomic://starterstack/account-network/peachtree-group/';
const atom = (id: string, path: string): CanvasAtomPosition => ({
  atom_id: id, source_url: prefix + path, x: 0, y: 0, title: id,
  primary_tag: null, tag_count: 1, tag_ids: ['peachtree'],
});
const link = (source_atom_id: string, target_atom_id: string | null, status = 'resolved'): AtomLink => ({
  source_atom_id, target_atom_id, status,
});

describe('accountNetworkEdges', () => {
  it('links the account to each executive and keeps sourced affiliations', () => {
    const atoms = [atom('account', 'account'), atom('greg', 'person/greg'), atom('jatin', 'person/jatin'),
      atom('alumni', 'door/alumni'), atom('school', 'school/texas')];
    const edges = accountNetworkEdges(atoms, [link('account', 'greg'), link('account', 'jatin'),
      link('account', 'alumni'), link('greg', 'school'), link('alumni', 'school')]);
    expect(edges.map(edge => [edge.source, edge.target, edge.relation])).toEqual([
      ['account', 'greg', 'account-executive'], ['account', 'jatin', 'account-executive'],
      ['account', 'alumni', 'research-door'], ['greg', 'school', 'education'],
      ['alumni', 'school', 'door-entity'],
    ]);
  });

  it('does not present unknown or unverified paths as relationships', () => {
    const atoms = [atom('account', 'account'), atom('greg', 'person/greg'),
      atom('network', 'door/network'), atom('unrelated', 'school/other')];
    expect(accountNetworkEdges(atoms, [link('greg', 'network'), link('account', 'unrelated'),
      link('account', 'greg', 'unresolved'), link('account', 'greg'), link('greg', 'account')]))
      .toEqual([{ source: 'account', target: 'greg', weight: 1, relation: 'account-executive' }]);
  });
});
