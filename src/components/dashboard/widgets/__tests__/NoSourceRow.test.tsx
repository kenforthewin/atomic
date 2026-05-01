import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, waitFor, cleanup } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { NoSourceRow } from '../review/NoSourceRow';

const invoke = vi.fn();
vi.mock('../../../../lib/transport', () => ({
  getTransport: () => ({ invoke }),
}));

describe('NoSourceRow', () => {
  beforeEach(() => {
    invoke.mockReset();
    invoke.mockResolvedValue({ status: 'ok' });
  });
  afterEach(() => cleanup());

  const atom = { id: 'a1', title: 'Meeting Notes', created_at: '2026-03-01T00:00:00Z' };

  it('renders title + date', () => {
    render(<NoSourceRow atom={atom} onResolved={() => {}} />);
    expect(screen.getByText('Meeting Notes')).toBeTruthy();
    expect(screen.getByText(/Created/i)).toBeTruthy();
  });

  it('saves a source URL via apply_health_item_fix', async () => {
    const onResolved = vi.fn();
    const user = userEvent.setup();
    render(<NoSourceRow atom={atom} onResolved={onResolved} />);
    await user.click(screen.getByText('Add source'));
    const input = screen.getByPlaceholderText('https://\u2026') as HTMLInputElement;
    await user.type(input, 'https://example.com');
    await user.click(screen.getByText('Save'));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('apply_health_item_fix', expect.objectContaining({
      check: 'content_quality',
      item_id: 'a1',
      action: 'add_source',
      url: 'https://example.com',
    })));
    await waitFor(() => expect(onResolved).toHaveBeenCalledWith('a1'), { timeout: 1000 });
  });

  it('marks intentional', async () => {
    const onResolved = vi.fn();
    const user = userEvent.setup();
    render(<NoSourceRow atom={atom} onResolved={onResolved} />);
    await user.click(screen.getByText('Intentional'));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('apply_health_item_fix', expect.objectContaining({
      check: 'content_quality',
      item_id: 'a1',
      action: 'mark_intentional',
    })));
  });

  it('shows error when save fails', async () => {
    invoke.mockRejectedValueOnce(new Error('nope'));
    const user = userEvent.setup();
    render(<NoSourceRow atom={atom} onResolved={() => {}} />);
    await user.click(screen.getByText('Add source'));
    await user.type(screen.getByPlaceholderText('https://\u2026'), 'x');
    await user.click(screen.getByText('Save'));
    await waitFor(() => expect(screen.getByText('nope')).toBeTruthy());
  });
});
