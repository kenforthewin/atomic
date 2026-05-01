import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, waitFor, cleanup } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { HealthReviewModal } from '../HealthReviewModal';

const invoke = vi.fn();
vi.mock('../../../../lib/transport', () => ({
  getTransport: () => ({ invoke }),
}));

// Mock tags store
vi.mock('../../../stores/tags', () => ({
  useTagsStore: (sel: (s: { tags: unknown[] }) => unknown) => sel({ tags: [] }),
}));

vi.mock('../../../stores/databases', () => ({
  useDatabasesStore: (sel: (s: { activeId: string }) => unknown) => sel({ activeId: 'default' }),
}));

const makeTagHealthReport = () => ({
  checks: {
    tag_health: {
      data: {
        rootless_tags: 0,
        similar_name_pairs: 0,
        single_atom_tags: 2,
        rootless_tag_list: [],
        similar_name_pair_list: [],
        single_atom_tag_list: [
          { id: 'tag-auto-1', name: 'AutoTag', is_autotag: true },
          { id: 'tag-manual-1', name: 'ManualTag', is_autotag: false },
        ],
      },
    },
  },
});

describe('TagHealthSection — single_atom_tag_list', () => {
  beforeEach(() => {
    invoke.mockReset();
    invoke.mockResolvedValue({ status: 'ok' });
    document.body.innerHTML = '';
  });

  afterEach(() => { cleanup(); });

  it('renders autotag with Delete button', () => {
    render(
      <HealthReviewModal
        report={makeTagHealthReport()}
        checkName="tag_health"
        onClose={vi.fn()}
        onResolved={vi.fn()}
      />,
    );
    expect(screen.getByText('AutoTag')).toBeTruthy();
    // Delete button only for autotag
    const deleteBtns = screen.getAllByText('Delete');
    expect(deleteBtns).toHaveLength(1);
  });

  it('does not render Delete button for manual tag', () => {
    render(
      <HealthReviewModal
        report={makeTagHealthReport()}
        checkName="tag_health"
        onClose={vi.fn()}
        onResolved={vi.fn()}
      />,
    );
    expect(screen.getByText('ManualTag')).toBeTruthy();
    // Manual tag should not have a Delete button — only autotag does
    const deleteBtns = screen.getAllByText('Delete');
    expect(deleteBtns).toHaveLength(1);
    // Manual tag should have merge dropdown
    expect(screen.getByText('Merge into\u2026')).toBeTruthy();
  });

  it('dispatches delete_tag for autotag Delete click', async () => {
    const user = userEvent.setup();
    render(
      <HealthReviewModal
        report={makeTagHealthReport()}
        checkName="tag_health"
        onClose={vi.fn()}
        onResolved={vi.fn()}
      />,
    );

    const deleteBtn = screen.getByText('Delete');
    await user.click(deleteBtn);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        'apply_health_item_fix',
        expect.objectContaining({
          check: 'tag_health',
          item_id: 'tag-auto-1',
          action: 'delete_tag',
        }),
      ),
    );
  });
});
