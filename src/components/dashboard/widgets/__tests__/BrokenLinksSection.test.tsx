import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, waitFor, cleanup, act } from '@testing-library/react';
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
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
    cleanup();
  });

  it('renders atom titles and link raws', () => {
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);
    expect(screen.getByText('First Atom')).toBeTruthy();
    expect(screen.getByText('Second Atom')).toBeTruthy();
    expect(screen.getByText('[[Missing Page]]')).toBeTruthy();
    expect(screen.getByText('[broken](./gone.md)')).toBeTruthy();
  });

  it('buttons are visible without hover', () => {
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);
    const removeBtns = screen.getAllByRole('button', { name: /Remove link/ });
    expect(removeBtns[0]).toBeTruthy();
    // no opacity-0 class on the wrapper
    expect(removeBtns[0].closest('[class*="opacity-0"]')).toBeNull();
  });

  it('dispatches remove_link with correct action and content on Remove link click', async () => {
    const onResolved = vi.fn();
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    render(<BrokenLinksSection data={makeData()} onResolved={onResolved} />);

    const removeBtns = screen.getAllByRole('button', { name: /Remove link/ });
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
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);

    const ignoreBtns = screen.getAllByRole('button', { name: /^Ignore$/ });
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
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);

    const ignoreAtomBtns = screen.getAllByRole('button', { name: /Ignore atom/ });
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

  it('Link… opens picker with link.target prefilled', async () => {
    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);

    const linkBtns = screen.getAllByRole('button', { name: /Link…/ });
    await user.click(linkBtns[0]);

    const input = screen.getByPlaceholderText('Search atoms…') as HTMLInputElement;
    expect(input).toBeTruthy();
    expect(input.value).toBe('Missing Page');
  });

  it('shows suggestions after typing query', async () => {
    invoke.mockImplementation((cmd: string) => {
      if (cmd === 'health_broken_link_suggest') {
        return Promise.resolve({ suggestions: [{ atom_id: 'atom-99', title: 'Found Atom', source_url: null, score: 0.9 }] });
      }
      return Promise.resolve({ status: 'ok' });
    });

    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);

    const linkBtns = screen.getAllByRole('button', { name: /Link…/ });
    await user.click(linkBtns[0]);

    const input = screen.getByPlaceholderText('Search atoms…');
    await user.clear(input);
    await user.type(input, 'Found');

    await act(async () => { vi.advanceTimersByTime(300); });

    await waitFor(() => {
      expect(invoke).toHaveBeenCalledWith('health_broken_link_suggest', expect.objectContaining({ q: 'Found', limit: 5 }));
    });

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

    const user = userEvent.setup({ advanceTimers: vi.advanceTimersByTime });
    render(<BrokenLinksSection data={makeData()} onResolved={vi.fn()} />);

    const linkBtns = screen.getAllByRole('button', { name: /Link…/ });
    await user.click(linkBtns[0]);

    const input = screen.getByPlaceholderText('Search atoms…');
    await user.clear(input);
    await user.type(input, 'Found');

    await act(async () => { vi.advanceTimersByTime(300); });

    await waitFor(() => expect(screen.getByText('Found Atom')).toBeTruthy());

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
