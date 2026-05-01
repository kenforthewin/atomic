import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { BoilerplateAtomRow } from '../review/BoilerplateAtomRow';

const invoke = vi.fn();
vi.mock('../../../../lib/transport', () => ({
  getTransport: () => ({ invoke }),
}));

describe('BoilerplateAtomRow', () => {
  beforeEach(() => {
    invoke.mockReset();
    invoke.mockResolvedValue({ status: 'ok' });
  });

  it('triggers re-embed', async () => {
    const onResolved = vi.fn();
    const user = userEvent.setup();
    render(<BoilerplateAtomRow atom={{ id: 'a1', title: 'x', clone_count: 3 }} onResolved={onResolved} />);
    await user.click(screen.getByText('Re-embed'));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('apply_health_item_fix', expect.objectContaining({
      check: 'boilerplate_pollution',
      item_id: 'a1',
      action: 'reembed',
    })));
    await waitFor(() => expect(onResolved).toHaveBeenCalledWith('a1'), { timeout: 1000 });
  });
});
