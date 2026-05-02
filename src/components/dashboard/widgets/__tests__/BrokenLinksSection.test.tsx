import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, waitFor, cleanup } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { BrokenLinksSection } from '../review/BrokenLinksSection';

const invoke = vi.fn();
vi.mock('../../../../lib/transport', () => ({
  getTransport: () => ({ invoke }),
}));

// Suppress toast errors from sonner (not mounted in test environment)
vi.mock('sonner', () => ({
  toast: Object.assign(vi.fn(), {
    error: vi.fn(),
    success: vi.fn(),
  }),
}));

// Suppress our toast wrapper
vi.mock('../../../../stores/toasts', () => ({
  toast: {
    error: vi.fn(),
    info: vi.fn(),
    success: vi.fn(),
  },
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

// ─── Tests that don't need fake timers ───────────────────────────────────────
describe('BrokenLinksSection', () => {
  beforeEach(() => {
    invoke.mockReset();
    invoke.mockResolvedValue({ status: 'ok' });
  });

  afterEach(() => {
    cleanup();
  });

  it('renders atom titles and link raws', () => {
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);
    expect(screen.getByText('First Atom')).toBeTruthy();
    expect(screen.getByText('Second Atom')).toBeTruthy();
    expect(screen.getByText('[[Missing Page]]')).toBeTruthy();
    expect(screen.getByText('[broken](./gone.md)')).toBeTruthy();
  });

  it('Auto-fix (LLM) and Remove buttons are visible', () => {
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);
    const autoFixBtns = screen.getAllByRole('button', { name: /Auto-fix with LLM/i });
    expect(autoFixBtns[0]).toBeTruthy();
    const removeBtns = screen.getAllByRole('button', { name: /Remove link/i });
    expect(removeBtns[0]).toBeTruthy();
  });

  it('dispatches remove_link with correct action and content on Remove link click', async () => {
    const onResolved = vi.fn();
    const user = userEvent.setup();
    render(<BrokenLinksSection data={makeData()} onResolved={onResolved} />);

    const removeBtns = screen.getAllByRole('button', { name: /Remove link/i });
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

    const ignoreBtns = screen.getAllByRole('button', { name: /^Ignore link$/i });
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

  it('per-row Auto-fix (LLM) calls auto_resolve with link raw', async () => {
    invoke.mockResolvedValue({ outcome: 'relinked', reason: 'Found a match' });
    const user = userEvent.setup();
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);

    const autoFixBtns = screen.getAllByRole('button', { name: /Auto-fix with LLM/i });
    await user.click(autoFixBtns[0]);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        'apply_health_item_fix',
        expect.objectContaining({
          check: 'broken_internal_links',
          item_id: 'atom-1',
          action: 'auto_resolve',
          content: '[[Missing Page]]',
        }),
      ),
    );
  });

  it('Auto-fix all button calls health_broken_links_auto_resolve_all', async () => {
    invoke.mockResolvedValue({ checked: 2, relinked: 1, removed: 1, skipped: 0 });
    const user = userEvent.setup();
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);

    const autoFixAllBtn = screen.getByRole('button', { name: /Auto-fix all broken links/i });
    await user.click(autoFixAllBtn);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith('health_broken_links_auto_resolve_all', {}),
    );
  });

  it('shows empty state when broken_link_list is empty', () => {
    render(<BrokenLinksSection data={{ broken_link_list: [] }} onResolved={vi.fn()} />);
    expect(screen.getByText(/No broken internal links/)).toBeTruthy();
  });

  it('Link… opens picker with link.target prefilled', async () => {
    const user = userEvent.setup();
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);

    const linkBtns = screen.getAllByRole('button', { name: /Link to atom/i });
    await user.click(linkBtns[0]);

    const input = screen.getByPlaceholderText('Search atoms…') as HTMLInputElement;
    expect(input).toBeTruthy();
    expect(input.value).toBe('Missing Page');
  });
});

// ─── Tests that need debounce (200 ms) ─────────────────────────────────────
// Use real timers — waitFor (default 1 s) covers the 200 ms debounce fine.
describe('BrokenLinksSection (debounce)', () => {
  beforeEach(() => {
    invoke.mockReset();
    invoke.mockResolvedValue({ status: 'ok' });
  });

  afterEach(() => {
    cleanup();
  });

  it('shows suggestions after typing query', async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === 'health_broken_link_suggest') {
        return Promise.resolve({ suggestions: [{ atom_id: 'atom-99', title: 'Found Atom', source_url: null, score: 0.9 }] });
      }
      return Promise.resolve({ status: 'ok' });
    });

    const user = userEvent.setup();
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);

    const linkBtns = screen.getAllByRole('button', { name: /Link to atom/i });
    await user.click(linkBtns[0]);

    const input = screen.getByPlaceholderText('Search atoms…');
    await user.clear(input);
    await user.type(input, 'Found');

    await waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('health_broken_link_suggest', expect.objectContaining({ q: 'Found', limit: 5 }));
    }, { timeout: 1000 });

    await waitFor(() => {
      expect(screen.getByText('Found Atom')).toBeTruthy();
    });
  });

  it('clicking suggestion dispatches relink with correct args', async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === 'health_broken_link_suggest') {
        return Promise.resolve({ suggestions: [{ atom_id: 'atom-99', title: 'Found Atom', source_url: null, score: 0.9 }] });
      }
      return Promise.resolve({ status: 'ok' });
    });

    const user = userEvent.setup();
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);

    const linkBtns = screen.getAllByRole('button', { name: /Link to atom/i });
    await user.click(linkBtns[0]);

    const input = screen.getByPlaceholderText('Search atoms…');
    await user.clear(input);
    await user.type(input, 'Found');

    await waitFor(() => expect(screen.getByText('Found Atom')).toBeTruthy(), { timeout: 1000 });

    await user.click(screen.getByText('Found Atom'));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        'apply_health_item_fix',
        expect.objectContaining({
          check: 'broken_internal_links',
          item_id: 'atom-1',
          action: 'relink',
          content: '[[Missing Page]]',
          into_tag_id: 'atom-99',
        }),
      ),
    );
  });
});
