import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, waitFor, cleanup } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

const invoke = vi.fn();
vi.mock('../../../lib/transport', () => ({
  getTransport: () => ({ invoke }),
}));

const { toastSuccess, toastError } = vi.hoisted(() => ({
  toastSuccess: vi.fn(),
  toastError: vi.fn(),
}));
vi.mock('../../../stores/toasts', () => ({
  toast: { error: toastError, info: vi.fn(), success: toastSuccess },
}));

vi.mock('../../../stores/tags', () => ({
  useTagsStore: (selector: (s: { tags: { id: string; name: string }[] }) => unknown) =>
    selector({ tags: [
      { id: 't1', name: 'research' },
      { id: 't2', name: 'draft' },
    ] }),
}));

import { CustomChecksPanel } from '../CustomChecksPanel';

async function renderWithInitial(initial: unknown[] = []) {
  invoke.mockImplementation((cmd: string) => {
    if (cmd === 'get_custom_health_checks') {
      return Promise.resolve({ checks: initial });
    }
    if (cmd === 'set_custom_health_checks') {
      return Promise.resolve();
    }
    return Promise.reject(new Error(`unexpected cmd: ${cmd}`));
  });
  const utils = render(<CustomChecksPanel />);
  // Wait for load to settle — empty-state copy appears only after loading=false.
  await waitFor(() =>
    expect(
      screen.queryByText(/Loading custom checks/i) ||
        screen.queryByText(/No custom checks yet/i) ||
        screen.queryAllByRole('textbox').length > 0,
    ).toBeTruthy(),
  );
  return utils;
}

describe('CustomChecksPanel', () => {
  beforeEach(() => {
    invoke.mockReset();
    toastSuccess.mockReset();
    toastError.mockReset();
  });

  afterEach(() => { cleanup(); });

  it('renders empty state when no checks exist', async () => {
    await renderWithInitial([]);
    await waitFor(() =>
      expect(screen.getByText(/No custom checks yet/i)).toBeTruthy(),
    );
  });

  it('loads checks from the server on mount', async () => {
    await renderWithInitial([
      {
        id: 'c1',
        label: 'Requires source',
        description: '',
        enabled: true,
        weight: 0.0,
        rule: { kind: 'require_source', tag_filter: null },
      },
    ]);
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('get_custom_health_checks', {}));
    const labelInput = await screen.findByDisplayValue('Requires source');
    expect(labelInput).toBeTruthy();
  });

  it('Add check appends a new row and marks dirty', async () => {
    const user = userEvent.setup();
    await renderWithInitial([]);
    const saveBtn = screen.getByRole('button', { name: /Saved/i });
    expect((saveBtn as HTMLButtonElement).disabled).toBe(true);

    await user.click(screen.getByRole('button', { name: /Add check/i }));
    expect(screen.getByDisplayValue('New check')).toBeTruthy();

    // Dirty flag flips → button label changes from "Saved" to "Save changes".
    await waitFor(() =>
      expect(screen.getByRole('button', { name: /Save changes/i })).toBeTruthy(),
    );
  });

  it('Delete removes the row and marks dirty', async () => {
    const user = userEvent.setup();
    await renderWithInitial([
      {
        id: 'c1',
        label: 'To delete',
        description: '',
        enabled: true,
        weight: 0.0,
        rule: { kind: 'require_source', tag_filter: null },
      },
    ]);
    expect(screen.getByDisplayValue('To delete')).toBeTruthy();
    await user.click(screen.getByRole('button', { name: /Delete/i }));
    expect(screen.queryByDisplayValue('To delete')).toBeNull();
    await waitFor(() =>
      expect(screen.getByRole('button', { name: /Save changes/i })).toBeTruthy(),
    );
  });

  it('Save clamps out-of-range weights and trims empty labels', async () => {
    const user = userEvent.setup();
    await renderWithInitial([
      {
        id: 'c1',
        label: '  ',
        description: '',
        enabled: true,
        weight: 2.5, // > 1 → clamped
        rule: { kind: 'require_source', tag_filter: null },
      },
    ]);

    // Loaded state is clean → flip dirty by toggling enabled, then save.
    await user.click(screen.getByLabelText('Enabled'));
    await user.click(await screen.findByRole('button', { name: /Save changes/i }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        'set_custom_health_checks',
        expect.objectContaining({
          checks: [
            expect.objectContaining({
              id: 'c1',
              label: 'Unnamed check',
              weight: 1.0,
            }),
          ],
        }),
      ),
    );
    expect(toastSuccess).toHaveBeenCalled();
  });

  it('Save surfaces a toast on error', async () => {
    invoke.mockImplementationOnce(() => Promise.resolve({ checks: [] }));
    invoke.mockImplementationOnce(() => Promise.reject(new Error('boom')));

    const user = userEvent.setup();
    render(<CustomChecksPanel />);
    await waitFor(() =>
      expect(screen.getByText(/No custom checks yet/i)).toBeTruthy(),
    );

    await user.click(screen.getByRole('button', { name: /Add check/i }));
    await user.click(screen.getByRole('button', { name: /Save changes/i }));

    await waitFor(() => expect(toastError).toHaveBeenCalled());
    expect(toastError.mock.calls[0][0]).toMatch(/Save custom checks failed/i);
  });

  it('switching rule kind rewrites the rule shape', async () => {
    const user = userEvent.setup();
    await renderWithInitial([
      {
        id: 'c1',
        label: 'Start',
        description: '',
        enabled: true,
        weight: 0.0,
        rule: { kind: 'require_source', tag_filter: null },
      },
    ]);

    // Rule-kind select is the first combobox inside the card.
    const selects = screen.getAllByRole('combobox');
    await user.selectOptions(selects[0], 'tag_cardinality');
    const minField = await screen.findByPlaceholderText('min');
    const maxField = await screen.findByPlaceholderText('max');
    expect((minField as HTMLInputElement).value).toBe('1');
    expect((maxField as HTMLInputElement).value).toBe('5');
  });

  it('Run preview invokes the preview command and shows counts', async () => {
    const user = userEvent.setup();
    invoke.mockImplementation((cmd: string) => {
      if (cmd === 'get_custom_health_checks') {
        return Promise.resolve({
          checks: [{
            id: 'c1',
            label: 'Needs source',
            description: '',
            enabled: true,
            weight: 0.0,
            rule: { kind: 'require_source', tag_filter: null },
          }],
        });
      }
      if (cmd === 'preview_custom_health_check') {
        return Promise.resolve({
          total_considered: 42,
          flagged_count: 7,
          sample: [
            { id: 'a1', title_preview: 'first atom' },
            { id: 'a2', title_preview: 'second atom' },
          ],
        });
      }
      return Promise.reject(new Error(`unexpected: ${cmd}`));
    });

    render(<CustomChecksPanel />);
    const runBtn = await screen.findByRole('button', { name: /Run preview/i });
    await user.click(runBtn);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        'preview_custom_health_check',
        expect.objectContaining({ rule: { kind: 'require_source', tag_filter: null } }),
      ),
    );
    expect(await screen.findByText(/Would flag/i)).toBeTruthy();
    expect(screen.getByText(/first atom/)).toBeTruthy();
    expect(screen.getByText(/second atom/)).toBeTruthy();
  });

  it('Preview error surfaces inline without toast', async () => {
    const user = userEvent.setup();
    invoke.mockImplementation((cmd: string) => {
      if (cmd === 'get_custom_health_checks') {
        return Promise.resolve({
          checks: [{
            id: 'c1',
            label: 'Bad',
            description: '',
            enabled: true,
            weight: 0.0,
            rule: { kind: 'content_regex', pattern: '(?P<x', invert: false },
          }],
        });
      }
      if (cmd === 'preview_custom_health_check') {
        return Promise.reject(new Error('invalid regex: unterminated'));
      }
      return Promise.reject(new Error(`unexpected: ${cmd}`));
    });

    render(<CustomChecksPanel />);
    await user.click(await screen.findByRole('button', { name: /Run preview/i }));

    const alert = await screen.findByRole('alert');
    expect(alert.textContent).toMatch(/invalid regex/i);
    expect(toastError).not.toHaveBeenCalled();
  });
});
