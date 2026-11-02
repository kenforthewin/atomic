import { describe, expect, it } from 'vitest';
import type { AtomLink, CanvasAtomPosition } from '../../lib/api';
import { accountResearchChecks, enrichedCategoryKeys, nodesWithoutRecordedRun, verifiedEntryPoints } from './accountNetworkEnrichment';

const prefix = 'atomic://starterstack/account-network/example/';
const atom = (id: string, path: string): CanvasAtomPosition => ({
  atom_id: id, source_url: prefix + path, x: 0, y: 0, title: id,
  primary_tag: null, tag_count: 1, tag_ids: ['example'],
});
const link = (source_atom_id: string, target_atom_id: string | null, status = 'resolved'): AtomLink => ({
  source_atom_id, target_atom_id, status,
});

describe('accountResearchChecks', () => {
  it('marks categories with recorded enrichment without counting empty door placeholders', () => {
    const atoms = [
      atom('greg', 'person/greg'), atom('network', 'door/network'), atom('alumni', 'door/alumni'),
      atom('communities', 'door/communities'), atom('board', 'door/board'), atom('agencies', 'door/agencies'),
      atom('account', 'account'),
    ];
    const notes = [
      { atomId: 'greg', content: 'Happenstance network check: 2026-11-01; source: https://happenstance.ai/chat/one.' },
      { atomId: 'network', content: 'Record type: People research door; checked: 2026-11-01.\nNamed public entities currently linked: 0.' },
      { atomId: 'alumni', content: 'Record type: People research door; checked: 2026-11-01.\nNamed public entities currently linked: 7.' },
      { atomId: 'communities', content: 'Record type: Partners research door; checked: 2026-11-01.\nNamed public entities currently linked: 3.' },
      { atomId: 'board', content: 'Record type: People research door; checked: 2026-11-01.\nNamed public entities currently linked: 0.' },
      { atomId: 'agencies', content: '- Enrichment run: Orange Slice | 2026-11-02 | empty | No named agency found' },
    ];
    const checks = accountResearchChecks(atoms, notes);
    expect([...enrichedCategoryKeys(atoms, checks)].sort()).toEqual([
      'agencies', 'alumni', 'communities', 'executive', 'network',
    ]);
  });

  it('separates profile data from a verified path and leaves empty door placeholders unrecorded', () => {
    const atoms = [atom('greg', 'person/greg'), atom('michael', 'person/michael'), atom('board', 'door/board'), atom('alumni', 'door/alumni')];
    const notes = [
      { atomId: 'greg', content: 'Common ground with Mark: no direct connection or named mutual found.\nHappenstance network check: 2026-11-01; source: https://happenstance.ai/chat/one.\nHappenstance-matched profile: https://happenstance.ai/u/greg.' },
      { atomId: 'michael', content: 'Happenstance network check: 2026-11-01; source: https://happenstance.ai/chat/two.\nCommon ground with Mark: no direct connection or named mutual found.' },
      { atomId: 'board', content: 'Record type: People research door; checked: 2026-11-01.\nNamed public entities currently linked: 0.' },
      { atomId: 'alumni', content: 'Record type: People research door; checked: 2026-11-01.\nNamed public entities currently linked: 7.' },
    ];
    const checks = accountResearchChecks(atoms, notes);
    expect(checks.map(check => [check.atomId, check.source, check.resultLabel, check.path])).toEqual([
      ['greg', 'Happenstance', 'Profile found', 'none'],
      ['alumni', 'Public research', '7 named entities', 'unknown'],
      ['michael', 'Happenstance', 'No exact profile recorded', 'none'],
    ]);
    expect(checks.find(check => check.atomId === 'greg')?.sourceUrl).toBe('https://happenstance.ai/chat/one');
    expect(nodesWithoutRecordedRun(atoms, checks).map(item => item.atom_id)).toEqual(['board']);
  });

  it('does not call a note without a recorded check an empty result', () => {
    expect(accountResearchChecks([atom('greg', 'person/greg')], [{ atomId: 'greg', content: 'Research pending' }])).toEqual([]);
  });

  it('keeps an empty run separate from a node with no recorded run', () => {
    const atoms = [atom('unchecked', 'person/unchecked'), atom('empty', 'person/empty')];
    const checks = accountResearchChecks(atoms, [{
      atomId: 'empty',
      content: '- Enrichment run: Happenstance | 2026-11-02 | empty | No profile found',
    }]);
    expect(checks).toHaveLength(1);
    expect(checks[0].result).toBe('empty');
    expect([...enrichedCategoryKeys(atoms, checks)]).toEqual(['executive']);
    expect(nodesWithoutRecordedRun(atoms, checks).map(item => item.atom_id)).toEqual(['unchecked']);
  });

  it('shows owner-observed LinkedIn mutuals separately from Happenstance coverage', () => {
    const notes = [{ atomId: 'greg', content: [
      '- Mutuals: LinkedIn shows Bo, Tony and 78 other mutual connections in Mark\'s view',
      '- LinkedIn mutuals evidence: Mark-provided screenshot; 2026-11-01; first-name labels only; introduction path unverified',
      'Happenstance network check: 2026-11-01; source: https://happenstance.ai/chat/one.',
      'Happenstance-matched profile: https://happenstance.ai/u/greg.',
    ].join('\n') }, { atomId: 'network', content: 'Record type: People research door; checked: 2026-11-01.\nNamed public entities currently linked: 0.' }];
    expect(accountResearchChecks([atom('greg', 'person/greg'), atom('network', 'door/network')], notes).map(check => [check.source, check.resultLabel, check.path])).toEqual([
      ['LinkedIn screenshot', 'Bo, Tony + 78 other mutual connections', 'unknown'],
      ['Happenstance', 'Profile found', 'unknown'],
    ]);
  });

  it('reads explicit provider runs on any account node without duplicating legacy checks', () => {
    const atoms = [atom('greg', 'person/greg'), atom('school', 'school/texas')];
    const notes = [
      { atomId: 'greg', content: '- Enrichment run: Happenstance | 2026-11-02 | empty | No named mutual | https://example.com/run\nHappenstance network check: 2026-11-01; source: https://example.com/old' },
      { atomId: 'school', content: '- Enrichment run: Alumni lookup | 2026-11-02 | data | 2 alumni records | https://example.com/alumni' },
    ];
    const checks = accountResearchChecks(atoms, notes);
    expect(checks).toHaveLength(2);
    expect(checks.map(check => [check.source, check.result, check.resultLabel])).toEqual([
      ['Alumni lookup', 'data', '2 alumni records'],
      ['Happenstance', 'empty', 'No named mutual'],
    ]);
  });
});

describe('verifiedEntryPoints', () => {
  it('requires an explicit declaration, source evidence, and a resolved atom link', () => {
    const atoms = [atom('greg', 'person/greg'), atom('mutual', 'door/network'), atom('other', 'person/other')];
    const notes = [{ atomId: 'greg', content: [
      '- Verified entry point: [[mutual|Named mutual]] ([Evidence](https://example.com/source))',
      '- Possible entry point: [[other|Other]] ([Evidence](https://example.com/other))',
    ].join('\n') }];
    expect(verifiedEntryPoints(atoms, notes, [link('greg', 'mutual'), link('greg', 'other')])).toEqual([
      { source: 'greg', target: 'mutual', evidenceUrl: 'https://example.com/source' },
    ]);
    expect(verifiedEntryPoints(atoms, notes, [link('greg', 'mutual', 'unresolved')])).toEqual([]);
  });

  it('does not animate an unsupported declaration or a link outside this account', () => {
    const atoms = [atom('greg', 'person/greg'), atom('mutual', 'door/network')];
    const notes = [{ atomId: 'greg', content: '- Verified entry point: [[mutual|Named mutual]]' }];
    expect(verifiedEntryPoints(atoms, notes, [link('greg', 'mutual')])).toEqual([]);
    expect(verifiedEntryPoints(atoms, [{ atomId: 'greg', content: '- Verified entry point: [[outside|Other]] ([Evidence](https://example.com))' }], [link('greg', 'outside')])).toEqual([]);
  });

  it('ignores example declarations inside a code fence', () => {
    const atoms = [atom('greg', 'person/greg'), atom('mutual', 'door/network')];
    const notes = [{ atomId: 'greg', content: '```markdown\n- Verified entry point: [[mutual|Example]] ([Evidence](https://example.com))\n```' }];
    expect(verifiedEntryPoints(atoms, notes, [link('greg', 'mutual')])).toEqual([]);
  });
});
