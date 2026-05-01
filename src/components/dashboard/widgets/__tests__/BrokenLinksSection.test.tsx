import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, waitFor, cleanup } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { BrokenLinksSection } from '../review/BrokenLinksSection';

const invoke = vi.fn();
vi.mock('../../../../lib/transport', () => ({
  getTransport: () => ({ invoke }),
}));

const makeData = () => ({
  broken_link_list: [
    {
      atom_id: 'atom-1',
      atom_title: 'First Atom',
      links: [{ raw: '[[Missing Page]]', target: 'Missing Page', kind: 'wikilink' }],
    },
    {
      atom_id: 'atom-2',
      atom_title: 'Second Atom',
      links: [{ raw: '[broken](./gone.md)', target: './gone.md', kind: 'markdown' }],
    },
  ],
});

describe('BrokenLinksSection', () => {
  beforeEach(() => {
    invoke.mockReset();
    invoke.mockResolvedValue({ status: 'ok' });
  });

  afterEach(() => { cleanup(); });

  it('renders atom titles and link raws', () => {
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);
    expect(screen.getByText('First Atom')).toBeTruthy();
    expect(screen.getByText('Second Atom')).toBeTruthy();
    expect(screen.getByText('[[Missing Page]]')).toBeTruthy();
    expect(screen.getByText('[broken](./gone.md)')).toBeTruthy();
  });

  it('dispatches remove_link with correct action and content on Remove link click', async () => {
    const onResolved = vi.fn();
    const user = userEvent.setup();
    render(<BrokenLinksSection data={makeData()} onResolved={onResolved} />);

    // Hover to reveal buttons — userEvent.hover triggers opacity
    const linkRow = screen.getByText('[[Missing Page]]').closest('.group');
    if (linkRow) {
      await user.hover(linkRow);
    }

    // Get all Remove link buttons, click the first one
    const removeBtns = screen.getAllByText('Remove link');
    await user.click(removeBtns[0]);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        'apply_health_item_fix',
        expect.objectContaining({
          check: 'broken_internal_links',
          item_id: 'atom-1',
          action: 'remove_link',
          content: '[[Missing Page]]',
        }),
      ),
    );
  });

  it('dispatches dismiss on Ignore click', async () => {
    const user = userEvent.setup();
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);

    const ignoreBtns = screen.getAllByText('Ignore');
    await user.click(ignoreBtns[0]);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        'apply_health_item_fix',
        expect.objectContaining({
          check: 'broken_internal_links',
          item_id: 'atom-1',
          action: 'dismiss',
        }),
      ),
    );
  });

  it('dispatches dismiss on Ignore atom click', async () => {
    const user = userEvent.setup();
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);

    const ignoreAtomBtns = screen.getAllByText('Ignore atom');
    await user.click(ignoreAtomBtns[0]);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        'apply_health_item_fix',
        expect.objectContaining({
          check: 'broken_internal_links',
          item_id: 'atom-1',
          action: 'dismiss',
        }),
      ),
    );
  });

  it('shows empty state when broken_link_list is empty', () => {
    render(<BrokenLinksSection data={{ broken_link_list: [] }} onResolved={vi.fn()} />);
    expect(screen.getByText(/No broken internal links/)).toBeTruthy();
  });
});
