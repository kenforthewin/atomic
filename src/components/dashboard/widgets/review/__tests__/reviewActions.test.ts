import { describe, it, expect, vi, beforeEach } from 'vitest';
import { runReviewAction } from '../reviewActions';

const invoke = vi.fn();
vi.mock('../../../../../lib/transport', () => ({
  getTransport: () => ({ invoke }),
}));

const { mockSonnerError } = vi.hoisted(() => ({
  mockSonnerError: vi.fn(),
}));

vi.mock('sonner', () => ({
  toast: Object.assign(vi.fn(), {
    error: mockSonnerError,
    success: vi.fn(),
  }),
}));

describe('runReviewAction', () => {
  beforeEach(() => {
    invoke.mockReset();
    vi.clearAllMocks();
  });

  it('returns invoke result on success', async () => {
    invoke.mockResolvedValue({ status: 'ok', count: 1 });
    const result = await runReviewAction({
      label: 'Remove link',
      command: 'apply_health_item_fix',
      args: { check: 'broken_internal_links', item_id: 'atom-1', action: 'remove_link' },
    });
    expect(result).toEqual({ status: 'ok', count: 1 });
    expect(invoke).toHaveBeenCalledWith('apply_health_item_fix', {
      check: 'broken_internal_links',
      item_id: 'atom-1',
      action: 'remove_link',
    });
  });

  it('returns undefined and fires toast.error on failure', async () => {
    invoke.mockRejectedValue(new Error('network error'));
    const result = await runReviewAction({
      label: 'Remove link',
      command: 'apply_health_item_fix',
      args: { item_id: 'atom-1', action: 'remove_link' },
    });
    expect(result).toBeUndefined();
    expect(mockSonnerError).toHaveBeenCalledWith(
      'Remove link failed',
      expect.objectContaining({ description: 'network error' }),
    );
  });

  it('toast on failure includes retry action', async () => {
    invoke.mockRejectedValue(new Error('bad'));
    await runReviewAction({
      label: 'Relink',
      command: 'apply_health_item_fix',
      args: { action: 'relink' },
    });
    const opts = mockSonnerError.mock.calls[0][1];
    expect(opts.action).toMatchObject({ label: 'Retry' });
  });
});
