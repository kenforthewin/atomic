import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, act, fireEvent } from '@testing-library/react';

const mockInvoke = vi.fn().mockResolvedValue({ status: 'ok' });

vi.mock('../../../../lib/transport', () => ({
  getTransport: () => ({ invoke: mockInvoke }),
}));

import { HealthReviewModal } from '../HealthReviewModal';

const makeReport = (similarPairs = [
  { pair_id: 'id-a__id-b', a_id: 'id-a', a_name: 'Machine Learning', b_id: 'id-b', b_name: 'Learning' },
]) => ({
  checks: {
    content_overlap: { data: { pairs: [], cross_source_overlaps: 0, count: 0 } },
    boilerplate_pollution: { data: { count: 0, affected_atoms: [], description: '' } },
    contradiction_detection: { data: { pairs_checked: 0, potential_contradictions: 0, pairs: [] } },
    content_quality: { data: { issues: { no_source: { count: 0, atoms: [] } } } },
    tag_health: {
      data: {
        rootless_tags: 0,
        similar_name_pairs: similarPairs.length,
        rootless_tag_list: [],
        similar_name_pair_list: similarPairs,
      },
    },
  },
});

describe('SimilarPairRow via HealthReviewModal (tag_health tab)', () => {
  const onClose = vi.fn();
  const onResolved = vi.fn();

  beforeEach(() => {
    vi.clearAllMocks();
    document.body.innerHTML = '';
  });

  it('renders pair names with Keep A and Keep B buttons', () => {
    render(
      <HealthReviewModal
        report={makeReport()}
        checkName="tag_health"
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    expect(screen.getByText('Machine Learning')).toBeTruthy();
    expect(screen.getByText('Learning')).toBeTruthy();
    expect(screen.getByText('Keep Machine Learning')).toBeTruthy();
    expect(screen.getByText('Keep Learning')).toBeTruthy();
    expect(screen.getByText('Ignore')).toBeTruthy();
  });

  it('merge Keep A calls apply_health_item_fix with into_tag_id = a_id', async () => {
    render(
      <HealthReviewModal
        report={makeReport()}
        checkName="tag_health"
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    const keepA = screen.getByText('Keep Machine Learning');
    await act(async () => { fireEvent.click(keepA); });
    expect(mockInvoke).toHaveBeenCalledWith(
      'apply_health_item_fix',
      expect.objectContaining({
        check: 'tag_health',
        item_id: 'id-a__id-b',
        action: 'merge_tags',
        into_tag_id: 'id-a',
      }),
    );
  });

  it('merge Keep B calls apply_health_item_fix with into_tag_id = b_id', async () => {
    render(
      <HealthReviewModal
        report={makeReport()}
        checkName="tag_health"
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    const keepB = screen.getByText('Keep Learning');
    await act(async () => { fireEvent.click(keepB); });
    expect(mockInvoke).toHaveBeenCalledWith(
      'apply_health_item_fix',
      expect.objectContaining({
        check: 'tag_health',
        item_id: 'id-a__id-b',
        action: 'merge_tags',
        into_tag_id: 'id-b',
      }),
    );
  });

  it('Ignore calls apply_health_item_fix with action dismiss', async () => {
    render(
      <HealthReviewModal
        report={makeReport()}
        checkName="tag_health"
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    const ignoreBtn = screen.getByText('Ignore');
    await act(async () => { fireEvent.click(ignoreBtn); });
    expect(mockInvoke).toHaveBeenCalledWith(
      'apply_health_item_fix',
      expect.objectContaining({
        check: 'tag_health',
        item_id: 'id-a__id-b',
        action: 'dismiss',
      }),
    );
  });
});
