import { describe, it, expect, vi, beforeEach } from 'vitest';
import { toast } from '../toasts';

// Must use vi.hoisted so variables are available in the hoisted vi.mock factory
const { mockSonnerError, mockSonnerSuccess, mockSonnerBase } = vi.hoisted(() => {
  const mockSonnerError = vi.fn();
  const mockSonnerSuccess = vi.fn();
  const mockSonnerBase = Object.assign(vi.fn(), {
    error: mockSonnerError,
    success: mockSonnerSuccess,
  });
  return { mockSonnerError, mockSonnerSuccess, mockSonnerBase };
});

vi.mock('sonner', () => ({
  toast: mockSonnerBase,
}));

describe('toast helpers', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('toast.error calls sonner.error with title', () => {
    toast.error('Something broke');
    expect(mockSonnerError).toHaveBeenCalledWith(
      'Something broke',
      expect.objectContaining({ duration: Infinity }),
    );
  });

  it('toast.error with detail passes description', () => {
    toast.error('Oops', { detail: 'Network timeout' });
    expect(mockSonnerError).toHaveBeenCalledWith(
      'Oops',
      expect.objectContaining({ description: 'Network timeout' }),
    );
  });

  it('toast.error with retry passes action with label Retry', () => {
    const retryFn = vi.fn();
    toast.error('Failed', { retry: retryFn });
    const call = mockSonnerError.mock.calls[0];
    expect(call[1]).toMatchObject({ action: { label: 'Retry' } });
    // Clicking action invokes retry
    call[1].action.onClick();
    expect(retryFn).toHaveBeenCalled();
  });

  it('toast.success uses undefined duration (sonner default)', () => {
    toast.success('Done');
    expect(mockSonnerSuccess).toHaveBeenCalledWith(
      'Done',
      expect.objectContaining({ duration: undefined }),
    );
  });

  it('toast.info calls base sonner with title', () => {
    toast.info('Heads up');
    expect(mockSonnerBase).toHaveBeenCalledWith(
      'Heads up',
      expect.objectContaining({ duration: undefined }),
    );
  });
});
