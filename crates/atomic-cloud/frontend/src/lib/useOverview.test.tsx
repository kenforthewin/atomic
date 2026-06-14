import { describe, it, expect, vi, beforeEach } from 'vitest';
import { renderHook, waitFor } from '@testing-library/react';
import { useOverview } from './useOverview';
import * as apiModule from './api';
import { ApiError } from './api';
import { overview } from '../test/fixtures';

const READY = overview({
  billing_state: 'trialing',
  trial_ends_at: '2026-06-28T00:00:00Z',
});

describe('useOverview', () => {
  beforeEach(() => {
    vi.restoreAllMocks();
  });

  it('resolves to ready with the overview', async () => {
    vi.spyOn(apiModule, 'getOverview').mockResolvedValue(READY);
    const { result } = renderHook(() => useOverview());
    await waitFor(() => expect(result.current.state.status).toBe('ready'));
    if (result.current.state.status !== 'ready') throw new Error('not ready');
    expect(result.current.state.overview.subdomain).toBe('alpha');
  });

  it('maps the account_provisioning 503 to the provisioning hold', async () => {
    vi.spyOn(apiModule, 'getOverview').mockRejectedValue(
      new ApiError(503, 'account_provisioning', 'Your account is being set up.'),
    );
    const { result } = renderHook(() => useOverview());
    await waitFor(() => expect(result.current.state.status).toBe('provisioning'));
  });

  it('maps the suspended 402 to a suspended state carrying the upgrade URL', async () => {
    vi.spyOn(apiModule, 'getOverview').mockRejectedValue(
      new ApiError(402, 'account_suspended', 'Suspended.', null, {
        error: 'account_suspended',
        upgrade_url: 'https://app.atomic.cloud/billing',
      }),
    );
    const { result } = renderHook(() => useOverview());
    await waitFor(() => expect(result.current.state.status).toBe('suspended'));
    if (result.current.state.status !== 'suspended') throw new Error('not suspended');
    expect(result.current.state.upgradeUrl).toBe('https://app.atomic.cloud/billing');
  });

  it('maps an unexpected error to the error state', async () => {
    vi.spyOn(apiModule, 'getOverview').mockRejectedValue(
      new ApiError(500, 'internal_error', 'Boom.'),
    );
    const { result } = renderHook(() => useOverview());
    await waitFor(() => expect(result.current.state.status).toBe('error'));
  });
});
