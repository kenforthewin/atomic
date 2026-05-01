import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { TagRootlessRow } from '../review/TagRootlessRow';

const invoke = vi.fn();
vi.mock('../../../../lib/transport', () => ({
  getTransport: () => ({ invoke }),
}));

describe('TagRootlessRow', () => {
  beforeEach(() => {
    invoke.mockReset();
    invoke.mockResolvedValue({ status: 'ok' });
  });

  const tag = { id: 't1', name: 'Foo', atom_count: 3 };
  const parents = [{ id: 'p1', name: 'Topics' }, { id: 'p2', name: 'People' }];

  it('moves tag under selected parent', async () => {
    const onResolved = vi.fn();
    const user = userEvent.setup();
    render(<TagRootlessRow tag={tag} parentOptions={parents} onResolved={onResolved} />);
    await user.selectOptions(screen.getByRole('combobox'), 'p1');
    const buttons = screen.getAllByRole('button');
    // Move button is the one that is not "Leave at root" dismiss
    const moveBtn = buttons.find(b => !b.hasAttribute('disabled') && b.getAttribute('title') !== "Leave at root \u2014 won't be flagged again");
    await user.click(moveBtn!);
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('apply_health_item_fix', expect.objectContaining({
      check: 'tag_health',
      item_id: 't1',
      action: 'move_under',
      parent_id: 'p1',
    })));
  });
});
