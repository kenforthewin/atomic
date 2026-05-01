import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, act } from '@testing-library/react';

const mockInvoke = vi.fn().mockResolvedValue({ content: '# content' });
vi.mock('../../../../lib/transport', () => ({
  getTransport: () => ({ invoke: mockInvoke }),
}));

import { HealthReviewModal } from '../HealthReviewModal';

const makeOverlapReport = () => ({
  checks: {
    content_overlap: {
      data: {
        pairs: [
          {
            pair_id: 'a1__b1',
            atom_a: { id: 'a1', title: 'Alpha Article', source: 'https://alpha.com' },
            atom_b: { id: 'b1', title: 'Beta Article', source: 'https://beta.com' },
            similarity: 0.72,
            shared_tag_count: 2,
            available_actions: ['keep_a', 'keep_b'],
          },
        ],
        cross_source_overlaps: 1,
        count: 1,
      },
    },
    boilerplate_pollution: { data: { count: 0, affected_atoms: [] } },
    contradiction_detection: { data: { pairs_checked: 0, potential_contradictions: 0, pairs: [] } },
    content_quality: { data: { issues: { no_source: { count: 0, atoms: [] } } } },
    tag_health: { data: { rootless_tags: 0, similar_name_pairs: 0, rootless_tag_list: [] } },
  },
});

describe('Markdown export (copyAsMarkdown)', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    document.body.innerHTML = '';
    Object.defineProperty(navigator, 'clipboard', {
      value: { writeText: vi.fn().mockResolvedValue(undefined) },
      writable: true,
      configurable: true,
    });
  });

  it('clipboard button calls navigator.clipboard.writeText with markdown containing headers and pairs', async () => {
    render(
      <HealthReviewModal
        report={makeOverlapReport()}
        checkName="content_overlap"
        onClose={vi.fn()}
        onResolved={vi.fn()}
      />,
    );
    const copyBtn = screen.getByTitle('Copy queue as markdown');
    await act(async () => { copyBtn.click(); });
    const written = (navigator.clipboard.writeText as ReturnType<typeof vi.fn>).mock.calls[0][0] as string;
    expect(written).toContain('# Health Review Queue');
    expect(written).toContain('## Content overlap');
    expect(written).toContain('Alpha Article');
    expect(written).toContain('Beta Article');
  });
});
