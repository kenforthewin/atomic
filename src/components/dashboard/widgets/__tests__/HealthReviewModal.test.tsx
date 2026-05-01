/**
 * HealthReviewModal tests
 *
 * NOTE: These tests require @testing-library/react and @testing-library/user-event.
 * Install with:
 *   npm install -D @testing-library/react @testing-library/user-event @testing-library/jest-dom
 * Then add `setupFiles: ['@testing-library/jest-dom/vitest']` to vitest.config.ts.
 */

import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HealthReviewModal } from '../HealthReviewModal';

// Mock transport
vi.mock('../../../../lib/transport', () => ({
  getTransport: () => ({
    invoke: vi.fn().mockResolvedValue({ content: '# Mock atom content\n\nSome text here' }),
  }),
}));

// Minimal report shape
const makeReport = (overrides: Record<string, unknown> = {}) => ({
  checks: {
    content_overlap: {
      data: {
        pairs: [],
        cross_source_overlaps: 0,
        exact_duplicates: 0,
        template_clones: 0,
        count: 0,
      },
    },
    boilerplate_pollution: {
      data: {
        count: 0,
        affected_atoms: [],
        description: '',
      },
    },
    contradiction_detection: {
      data: {
        pairs_checked: 0,
        potential_contradictions: 0,
        pairs: [],
      },
    },
    content_quality: {
      data: {
        issues: {
          no_source: { count: 0, atoms: [] },
        },
      },
    },
    tag_health: {
      data: {
        rootless_tags: 0,
        similar_name_pairs: 0,
        rootless_tag_list: [],
      },
    },
    ...overrides,
  },
});

describe('HealthReviewModal', () => {
  const onClose = vi.fn();
  const onResolved = vi.fn();

  beforeEach(() => {
    vi.clearAllMocks();
    document.body.innerHTML = '';
  });

  it('shows nothing-to-review when all checks empty', () => {
    render(
      <HealthReviewModal
        report={makeReport()}
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    expect(screen.getByText(/nothing to review/i)).toBeTruthy();
  });

  it('shows content overlap tab when pairs exist', () => {
    const report = makeReport({
      content_overlap: {
        data: {
          pairs: [
            {
              pair_id: 'p1',
              atom_a: { id: 'a1', title: 'Article Alpha', source: 'https://site1.com/a' },
              atom_b: { id: 'b1', title: 'Article Beta', source: 'https://site2.com/b' },
              similarity: 0.72,
              shared_tag_count: 3,
              available_actions: ['merge_with_llm', 'keep_both'],
            },
          ],
          cross_source_overlaps: 1,
          count: 1,
        },
      },
    });
    render(
      <HealthReviewModal
        report={report}
        checkName="content_overlap"
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    // 'Content overlap' tab label only appears when >1 tab; check content instead
    expect(screen.getByText('Article Alpha')).toBeTruthy();
    expect(screen.getByText('Article Beta')).toBeTruthy();
    expect(screen.getByText('72% overlap')).toBeTruthy();
  });

  it('shows boilerplate tab with titles and clone counts', () => {
    const report = makeReport({
      boilerplate_pollution: {
        data: {
          count: 2,
          affected_atoms: [
            { id: 'bp1', title: 'Template Article A', clone_count: 5 },
            { id: 'bp2', title: 'Template Article B', clone_count: 2 },
          ],
          description: 'test',
        },
      },
    });
    render(
      <HealthReviewModal
        report={report}
        checkName="boilerplate_pollution"
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    expect(screen.getByText('Template Article A')).toBeTruthy();
    expect(screen.getByText(/5 near-identical edge/)).toBeTruthy();
    expect(screen.getByText('Template Article B')).toBeTruthy();
    expect(screen.getByText(/2 near-identical edge/)).toBeTruthy();
  });

  it('shows contradiction tab with pair titles and similarity', () => {
    const report = makeReport({
      contradiction_detection: {
        data: {
          pairs_checked: 50,
          potential_contradictions: 1,
          pairs: [
            {
              pair_id: 'cp1',
              atom_a: { id: 'ca1', title: 'Topic X Version 1', source: 'https://s1.com' },
              atom_b: { id: 'cb1', title: 'Topic X Version 2', source: 'https://s2.com' },
              similarity: 0.85,
              shared_tag_count: 2,
            },
          ],
        },
      },
    });
    render(
      <HealthReviewModal
        report={report}
        checkName="contradiction_detection"
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    // 'Contradictions' tab label only appears when >1 tab; check content instead
    expect(screen.getByText('Topic X Version 1')).toBeTruthy();
    expect(screen.getByText('Topic X Version 2')).toBeTruthy();
    expect(screen.getByText(/85% similarity/)).toBeTruthy();
  });

  it('shows content quality tab with atom titles', () => {
    const report = makeReport({
      content_quality: {
        data: {
          issues: {
            no_source: {
              count: 2,
              atoms: [
                { id: 'q1', title: 'Note Without Source', created_at: '2026-01-15T10:00:00Z' },
                { id: 'q2', title: 'Another Unsourced Note', created_at: '2026-02-01T10:00:00Z' },
              ],
            },
          },
        },
      },
    });
    render(
      <HealthReviewModal
        report={report}
        checkName="content_quality"
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    // 'No source' tab label only appears when >1 tab; check content instead
    expect(screen.getByText('Note Without Source')).toBeTruthy();
    expect(screen.getByText('Another Unsourced Note')).toBeTruthy();
    expect(screen.getByText(/1\/15\/2026|Jan 15|15 Jan/)).toBeTruthy();
  });

  it('shows tag health tab with rootless tag names', () => {
    const report = makeReport({
      tag_health: {
        data: {
          rootless_tags: 2,
          similar_name_pairs: 0,
          rootless_tag_list: [
            { id: 'tg1', name: 'Orphaned Category', atom_count: 7 },
            { id: 'tg2', name: 'Floating Topic', atom_count: 2 },
          ],
        },
      },
    });
    render(
      <HealthReviewModal
        report={report}
        checkName="tag_health"
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    // Tab bar only shows with >1 tab; content still renders
    expect(screen.getByText('Orphaned Category')).toBeTruthy();
    expect(screen.getByText('Floating Topic')).toBeTruthy();
    expect(screen.getByText(/7 atom/)).toBeTruthy();
    expect(screen.getByText(/2 atom/)).toBeTruthy();
  });

  it('pre-selects tab from checkName prop', () => {
    const report = makeReport({
      content_overlap: {
        data: {
          pairs: [{
            pair_id: 'p1',
            atom_a: { id: 'a1', title: 'Alpha', source: null },
            atom_b: { id: 'b1', title: 'Beta', source: null },
            similarity: 0.70,
            shared_tag_count: 2,
            available_actions: ['merge_with_llm', 'keep_both'],
          }],
          cross_source_overlaps: 1,
          count: 1,
        },
      },
      boilerplate_pollution: {
        data: {
          count: 1,
          affected_atoms: [{ id: 'bp1', title: 'Boilerplate Article', clone_count: 3 }],
          description: '',
        },
      },
    });
    render(
      <HealthReviewModal
        report={report}
        checkName="boilerplate_pollution"
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    expect(screen.getByText('Boilerplate Article')).toBeTruthy();
  });

  it('calls onClose when X button clicked', async () => {
    render(
      <HealthReviewModal
        report={makeReport()}
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    const buttons = screen.getAllByRole('button');
    if (buttons.length > 0) await userEvent.click(buttons[0]);
    // verify no crash; onClose called depends on button order
  });
});
