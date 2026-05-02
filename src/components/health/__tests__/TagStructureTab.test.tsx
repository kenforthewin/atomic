import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, waitFor, cleanup } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

const invoke = vi.fn();
vi.mock('../../../lib/transport', () => ({
  getTransport: () => ({ invoke }),
}));

vi.mock('../../../stores/toasts', () => ({
  toast: { error: vi.fn(), info: vi.fn(), success: vi.fn() },
}));

const fetchTags = vi.fn();
vi.mock('../../../stores/tags', () => ({
  useTagsStore: (selector: (s: Record<string, unknown>) => unknown) => selector({ fetchTags }),
}));

import { TagStructureTab } from '../TagStructureTab';

const makeProposal = () => ({
  id: 'prop-1',
  summary: 'Found 3 opportunities to consolidate tags',
  actions: [
    { kind: 'merge', from_id: 't1', into_id: 't2', from_name: 'k8s', into_name: 'Kubernetes', reason: 'Same concept' },
    { kind: 'rename', tag_id: 't3', old_name: 'Kub', new_name: 'Kubernetes', reason: 'Typo correction' },
    { kind: 'delete', tag_id: 't4', tag_name: 'temp_tag', reason: 'Unused' },
  ],
  generated_at: new Date().toISOString(),
});

describe('TagStructureTab — no proposal', () => {
  beforeEach(() => {
    invoke.mockReset();
    // 404 error means no proposal
    invoke.mockRejectedValue(Object.assign(new Error('Not found'), { status: 404 }));
  });

  afterEach(() => { cleanup(); });

  it('shows empty state with propose button', async () => {
    render(<TagStructureTab />);
    await waitFor(() => expect(screen.getByText(/No tag restructure proposal/i)).toBeTruthy());
    expect(screen.getByRole('button', { name: /Propose tag restructure with LLM/i })).toBeTruthy();
  });

  it('calls health_tag_proposal_create on propose button click', async () => {
    invoke.mockRejectedValueOnce(Object.assign(new Error('Not found'), { status: 404 }));
    invoke.mockResolvedValueOnce(makeProposal());
    const user = userEvent.setup();
    render(<TagStructureTab />);
    await waitFor(() => expect(screen.getByText(/No tag restructure proposal/i)).toBeTruthy());

    await user.click(screen.getByRole('button', { name: /Propose tag restructure with LLM/i }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith('health_tag_proposal_create', {}));
  });
});

describe('TagStructureTab — with proposal', () => {
  beforeEach(() => {
    invoke.mockReset();
    invoke.mockResolvedValue(makeProposal());
  });

  afterEach(() => { cleanup(); });

  it('renders proposal summary and action list', async () => {
    render(<TagStructureTab />);
    await waitFor(() => expect(screen.getByText(/Found 3 opportunities/i)).toBeTruthy());
    expect(screen.getByText(/Merge "k8s" into "Kubernetes"/i)).toBeTruthy();
    expect(screen.getByText(/Rename "Kub" → "Kubernetes"/i)).toBeTruthy();
    expect(screen.getByText(/Delete "temp_tag"/i)).toBeTruthy();
  });

  it('all actions pre-checked', async () => {
    render(<TagStructureTab />);
    await waitFor(() => expect(screen.getByText(/Found 3 opportunities/i)).toBeTruthy());
    const checkboxes = screen.getAllByRole('checkbox') as HTMLInputElement[];
    // First checkbox is "select all", rest are per-action
    const actionBoxes = checkboxes.slice(1);
    expect(actionBoxes.every(cb => cb.checked)).toBe(true);
  });

  it('Apply selected calls health_tag_proposal_apply with accepted_indices', async () => {
    invoke.mockResolvedValueOnce(makeProposal()); // initial load
    invoke.mockResolvedValueOnce([{ id: 'fix-1', check: 'tag_health', action: 'merge', count: 1, details: [] }]); // apply
    const user = userEvent.setup();
    render(<TagStructureTab />);
    await waitFor(() => expect(screen.getByText(/Found 3 opportunities/i)).toBeTruthy());

    const applyBtn = screen.getByRole('button', { name: /Apply.*selected/i });
    await user.click(applyBtn);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        'health_tag_proposal_apply',
        expect.objectContaining({
          proposal_id: 'prop-1',
          accepted_indices: [0, 1, 2],
        }),
      ),
    );
  });

  it('unchecking an action excludes it from apply', async () => {
    invoke.mockResolvedValueOnce(makeProposal());
    invoke.mockResolvedValueOnce([]);
    const user = userEvent.setup();
    render(<TagStructureTab />);
    await waitFor(() => expect(screen.getByText(/Found 3 opportunities/i)).toBeTruthy());

    // Uncheck first action (index 0)
    const checkboxes = screen.getAllByRole('checkbox') as HTMLInputElement[];
    const firstActionCheckbox = checkboxes[1]; // 0 is select-all
    await user.click(firstActionCheckbox);

    const applyBtn = screen.getByRole('button', { name: /Apply.*selected/i });
    await user.click(applyBtn);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        'health_tag_proposal_apply',
        expect.objectContaining({
          proposal_id: 'prop-1',
          accepted_indices: [1, 2],
        }),
      ),
    );
  });
});
