import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, waitFor, cleanup } from '@testing-library/react';
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

  afterEach(() => { cleanup(); });

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

  it('Strip… shows diff preview after dry_run call', async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === 'get_atom') return Promise.resolve({ content: 'Original content here' });
      if (cmd === 'health_strip_boilerplate') return Promise.resolve({ content: 'Stripped content here' });
      return Promise.resolve({ status: 'ok' });
    });
    const user = userEvent.setup();
    render(<BoilerplateAtomRow atom={{ id: 'a2', title: 'Test', clone_count: 2 }} onResolved={vi.fn()} />);
    await user.click(screen.getByTitle('Ask LLM to remove template boilerplate, keep unique content'));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('health_strip_boilerplate', expect.objectContaining({ atom_id: 'a2', dry_run: true })));
    await waitFor(() => expect(screen.getByText('Preview — apply to update the atom')).toBeTruthy());
    expect(screen.getByText('Apply strip')).toBeTruthy();
    expect(screen.getByText('Cancel')).toBeTruthy();
  });

  it('Cancel button hides the strip preview', async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === 'get_atom') return Promise.resolve({ content: 'Original' });
      if (cmd === 'health_strip_boilerplate') return Promise.resolve({ content: 'Stripped' });
      return Promise.resolve({ status: 'ok' });
    });
    const user = userEvent.setup();
    render(<BoilerplateAtomRow atom={{ id: 'a3', title: 'Test', clone_count: 1 }} onResolved={vi.fn()} />);
    await user.click(screen.getByTitle('Ask LLM to remove template boilerplate, keep unique content'));
    await waitFor(() => screen.getByText('Cancel'));
    await user.click(screen.getByText('Cancel'));
    expect(screen.queryByText('Apply strip')).toBeNull();
  });
});
