import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, act } from '@testing-library/react';

// Must mock before importing the component under test
const mockInvoke = vi.fn().mockResolvedValue({ content: '# content A\n\nBody' });

vi.mock('../../../../lib/transport', () => ({
  getTransport: () => ({ invoke: mockInvoke }),
}));

// PairRow is not exported; we test it via the modal with content_overlap pairs
import { HealthReviewModal } from '../HealthReviewModal';

const makePair = (overrides = {}) => ({
  pair_id: 'p1',
  atom_a: { id: 'a1', title: 'Article Alpha', source: 'https://site1.com/a', created_at: '2024-01-01T00:00:00Z' },
  atom_b: { id: 'b1', title: 'Article Beta', source: 'https://site2.com/b', created_at: '2025-01-01T00:00:00Z' },
  similarity: 0.72,
  shared_tag_count: 3,
  available_actions: ['keep_a', 'keep_b', 'merge_with_edited_content'],
  ...overrides,
});

const makeReport = (pairs = [makePair()], contradictionPairs: unknown[] = []) => ({
  checks: {
    content_overlap: { data: { pairs, cross_source_overlaps: 1, count: pairs.length } },
    boilerplate_pollution: { data: { count: 0, affected_atoms: [], description: '' } },
    contradiction_detection: { data: { pairs_checked: 0, potential_contradictions: contradictionPairs.length, pairs: contradictionPairs } },
    content_quality: { data: { issues: { no_source: { count: 0, atoms: [] } } } },
    tag_health: { data: { rootless_tags: 0, similar_name_pairs: 0, rootless_tag_list: [] } },
  },
});

describe('PairRow via HealthReviewModal', () => {
  const onClose = vi.fn();
  const onResolved = vi.fn();

  beforeEach(() => {
    vi.clearAllMocks();
    document.body.innerHTML = '';
  });

  it('renders Keep A, Keep B, Merge… buttons', () => {
    render(
      <HealthReviewModal
        report={makeReport()}
        checkName="content_overlap"
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    expect(screen.getByTitle('Delete the right atom; keep the left one')).toBeTruthy();
    expect(screen.getByTitle('Delete the left atom; keep the right one')).toBeTruthy();
    expect(screen.getByTitle('Open an editor to combine both atoms, then delete the loser')).toBeTruthy();
  });

  it('Keep A button triggers apply_health_item_fix with keep_a', async () => {
    render(
      <HealthReviewModal
        report={makeReport()}
        checkName="content_overlap"
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    const keepABtn = screen.getByTitle('Delete the right atom; keep the left one');
    await act(async () => { fireEvent.click(keepABtn); });
    expect(mockInvoke).toHaveBeenCalledWith(
      'apply_health_item_fix',
      expect.objectContaining({ action: 'keep_a' }),
    );
  });

  it('Keep B button triggers apply_health_item_fix with keep_b', async () => {
    render(
      <HealthReviewModal
        report={makeReport()}
        checkName="content_overlap"
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    const keepBBtn = screen.getByTitle('Delete the left atom; keep the right one');
    await act(async () => { fireEvent.click(keepBBtn); });
    expect(mockInvoke).toHaveBeenCalledWith(
      'apply_health_item_fix',
      expect.objectContaining({ action: 'keep_b' }),
    );
  });
});

describe('ContradictionRow Flag for later', () => {
  const onClose = vi.fn();
  const onResolved = vi.fn();

  beforeEach(() => {
    vi.clearAllMocks();
    document.body.innerHTML = '';
  });

  it('Flag for later calls apply_health_item_fix with defer', async () => {
    const report = makeReport([], [
      {
        pair_id: 'cp1',
        atom_a: { id: 'ca1', title: 'Topic X V1', source: 'https://s1.com', created_at: '2024-01-01T00:00:00Z' },
        atom_b: { id: 'cb1', title: 'Topic X V2', source: 'https://s2.com', created_at: '2025-01-01T00:00:00Z' },
        similarity: 0.85,
        shared_tag_count: 2,
      },
    ]);
    render(
      <HealthReviewModal
        report={report}
        checkName="contradiction_detection"
        onClose={onClose}
        onResolved={onResolved}
      />
    );
    const flagBtn = screen.getByTitle('Hide this pair for 7 days');
    await act(async () => { fireEvent.click(flagBtn); });
    expect(mockInvoke).toHaveBeenCalledWith(
      'apply_health_item_fix',
      expect.objectContaining({ action: 'defer', check: 'contradiction_detection' }),
    );
  });
});


describe('Content overlap batch selection checkbox', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    document.body.innerHTML = '';
  });

  it('checking a pair checkbox shows the batch footer', async () => {
    render(
      <HealthReviewModal
        report={makeReport()}
        checkName="content_overlap"
        onClose={vi.fn()}
        onResolved={vi.fn()}
      />,
    );
    const checkbox = document.querySelector('input[type="checkbox"]') as HTMLInputElement;
    expect(checkbox).toBeTruthy();
    await act(async () => { fireEvent.click(checkbox); });
    expect(screen.getByText(/1 selected/)).toBeTruthy();
    expect(screen.getByText(/Dismiss 1/)).toBeTruthy();
  });

  it('Clear button removes selection', async () => {
    render(
      <HealthReviewModal
        report={makeReport()}
        checkName="content_overlap"
        onClose={vi.fn()}
        onResolved={vi.fn()}
      />,
    );
    const checkbox = document.querySelector('input[type="checkbox"]') as HTMLInputElement;
    await act(async () => { fireEvent.click(checkbox); });
    const clearBtn = screen.getByText('Clear');
    await act(async () => { fireEvent.click(clearBtn); });
    expect(screen.queryByText(/1 selected/)).toBeNull();
  });
});

describe('PairRow — Verify with LLM (content_overlap)', () => {
  beforeEach(() => {
    vi.clearAllMocks();
    document.body.innerHTML = '';
  });

  it('Verify with LLM button dispatches verify_with_llm action', async () => {
    mockInvoke.mockResolvedValue({ is_duplicate: true, reason: 'Same content' });
    render(
      <HealthReviewModal
        report={makeReport()}
        checkName="content_overlap"
        onClose={vi.fn()}
        onResolved={vi.fn()}
      />
    );
    const verifyBtn = screen.getByTitle('Ask the LLM whether this is a real duplicate');
    await act(async () => { fireEvent.click(verifyBtn); });
    expect(mockInvoke).toHaveBeenCalledWith(
      'apply_health_item_fix',
      expect.objectContaining({ action: 'verify_with_llm', check: 'content_overlap' }),
    );
  });

  it('Merge with LLM button dispatches merge_with_llm action', async () => {
    mockInvoke.mockResolvedValue({ status: 'ok' });
    render(
      <HealthReviewModal
        report={makeReport()}
        checkName="content_overlap"
        onClose={vi.fn()}
        onResolved={vi.fn()}
      />
    );
    const mergeBtn = screen.getByTitle('LLM merges both atoms into one reconciled document');
    await act(async () => { fireEvent.click(mergeBtn); });
    expect(mockInvoke).toHaveBeenCalledWith(
      'apply_health_item_fix',
      expect.objectContaining({ action: 'merge_with_llm', check: 'content_overlap' }),
    );
  });
});

describe('ContradictionRow — Verify/Resolve with LLM', () => {
  const makeContradictionReport = () => makeReport([], [
    {
      pair_id: 'cp1',
      atom_a: { id: 'ca1', title: 'Topic X V1', source: 'https://s1.com', created_at: '2024-01-01T00:00:00Z' },
      atom_b: { id: 'cb1', title: 'Topic X V2', source: 'https://s2.com', created_at: '2025-01-01T00:00:00Z' },
      similarity: 0.85,
      shared_tag_count: 2,
    },
  ]);

  beforeEach(() => {
    vi.clearAllMocks();
    document.body.innerHTML = '';
  });

  it('Verify (LLM) button dispatches verify_with_llm for contradiction', async () => {
    mockInvoke.mockResolvedValue({ is_real: false, reason: 'Not a real contradiction' });
    render(
      <HealthReviewModal
        report={makeContradictionReport()}
        checkName="contradiction_detection"
        onClose={vi.fn()}
        onResolved={vi.fn()}
      />
    );
    const verifyBtn = screen.getByTitle('Ask LLM if this is a real contradiction');
    await act(async () => { fireEvent.click(verifyBtn); });
    expect(mockInvoke).toHaveBeenCalledWith(
      'apply_health_item_fix',
      expect.objectContaining({ action: 'verify_with_llm', check: 'contradiction_detection' }),
    );
  });

  it('Resolve (LLM) button dispatches merge_with_llm for contradiction', async () => {
    mockInvoke.mockResolvedValue({ status: 'ok' });
    render(
      <HealthReviewModal
        report={makeContradictionReport()}
        checkName="contradiction_detection"
        onClose={vi.fn()}
        onResolved={vi.fn()}
      />
    );
    const resolveBtn = screen.getByTitle('LLM merges both atoms into one reconciled document');
    await act(async () => { fireEvent.click(resolveBtn); });
    expect(mockInvoke).toHaveBeenCalledWith(
      'apply_health_item_fix',
      expect.objectContaining({ action: 'merge_with_llm', check: 'contradiction_detection' }),
    );
  });
});