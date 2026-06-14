import { describe, it, expect, vi, afterEach, beforeEach } from 'vitest';
import { billingDescriptor, billingNotice, startBillingFlow, BillingFlowError } from './billing';
import type { BillingState } from './api';

const ALL_STATES: BillingState[] = [
  'active',
  'trialing',
  'past_due',
  'read_only',
  'suspended',
];

describe('billingDescriptor', () => {
  it('maps each state to its status-pill tone and label', () => {
    expect(billingDescriptor('active')).toEqual({ tone: 'success', label: 'Active' });
    expect(billingDescriptor('trialing')).toEqual({ tone: 'accent', label: 'Trial' });
    expect(billingDescriptor('past_due')).toEqual({ tone: 'warning', label: 'Past due' });
    expect(billingDescriptor('read_only')).toEqual({ tone: 'warning', label: 'Read-only' });
    expect(billingDescriptor('suspended')).toEqual({ tone: 'error', label: 'Suspended' });
  });

  it('returns a descriptor for every billing state (exhaustive)', () => {
    for (const state of ALL_STATES) {
      expect(billingDescriptor(state).label).toBeTruthy();
    }
  });
});

describe('billingNotice', () => {
  afterEach(() => {
    vi.useRealTimers();
  });

  it('renders no banner for an active account', () => {
    expect(billingNotice('active', null)).toBeNull();
  });

  it('selects the right banner tone for each non-active state', () => {
    expect(billingNotice('trialing', null)?.tone).toBe('info');
    expect(billingNotice('past_due', null)?.tone).toBe('warning');
    expect(billingNotice('read_only', null)?.tone).toBe('warning');
    expect(billingNotice('suspended', null)?.tone).toBe('error');
  });

  it('offers a "Manage billing" action on every recoverable state', () => {
    for (const state of ALL_STATES) {
      const notice = billingNotice(state, null);
      if (state === 'active') {
        expect(notice).toBeNull();
      } else {
        expect(notice?.action).toBe(true);
      }
    }
  });

  it('counts the trial days into the title from a fixed now', () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date('2026-06-14T00:00:00Z'));

    expect(billingNotice('trialing', '2026-06-17T00:00:00Z')?.title).toBe(
      '3 days left in your free trial.',
    );
    // Singular day.
    expect(billingNotice('trialing', '2026-06-15T00:00:00Z')?.title).toBe(
      '1 day left in your free trial.',
    );
    // Ends today (now or already past the deadline within the day window).
    expect(billingNotice('trialing', '2026-06-14T00:00:00Z')?.title).toBe(
      'Your free trial ends today.',
    );
    // Unknown deadline → a neutral "active" title, no day count.
    expect(billingNotice('trialing', null)?.title).toBe('Your free trial is active.');
  });

  it('keeps the read-only and suspended copy reassuring about data retention', () => {
    expect(billingNotice('read_only', null)?.body).toMatch(/data is safe/i);
    expect(billingNotice('suspended', null)?.body).toMatch(/retained/i);
  });
});

describe('startBillingFlow', () => {
  const realFetch = globalThis.fetch;
  const realLocation = window.location;
  let assign: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    assign = vi.fn();
    // jsdom's `location.assign` is non-configurable, so swap the whole object
    // for a mock that records the navigation target.
    Object.defineProperty(window, 'location', {
      configurable: true,
      value: { ...realLocation, assign },
    });
  });

  afterEach(() => {
    globalThis.fetch = realFetch;
    Object.defineProperty(window, 'location', {
      configurable: true,
      value: realLocation,
    });
    vi.restoreAllMocks();
  });

  it('follows an opaque cross-origin redirect with a top-level navigation', async () => {
    // The real Stripe 302 surfaces as an opaque redirect (status 0, type
    // 'opaqueredirect') under `redirect: 'manual'`. The Response ctor can't
    // mint that type, so stand in a minimal shape the helper reads.
    globalThis.fetch = vi
      .fn()
      .mockResolvedValue({ type: 'opaqueredirect', status: 0 }) as unknown as typeof fetch;

    // Never resolves on success — the navigation supersedes it — so race it.
    const flow = startBillingFlow('/api/billing/checkout?plan=pro');
    await Promise.race([flow, new Promise((r) => setTimeout(r, 10))]);

    expect(assign).toHaveBeenCalledWith('/api/billing/checkout?plan=pro');
  });

  it('surfaces a friendly message for an unmapped plan (unknown_plan JSON)', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ error: 'unknown_plan', message: 'No such purchasable plan.' }), {
        status: 400,
        headers: { 'Content-Type': 'application/json' },
      }),
    ) as unknown as typeof fetch;

    await expect(startBillingFlow('/api/billing/checkout?plan=pro')).rejects.toBeInstanceOf(
      BillingFlowError,
    );
    // Never navigates onto the raw JSON.
    expect(assign).not.toHaveBeenCalled();
  });

  it('maps billing_not_configured to a friendly in-app message', async () => {
    globalThis.fetch = vi.fn().mockResolvedValue(
      new Response(JSON.stringify({ error: 'billing_not_configured' }), { status: 503 }),
    ) as unknown as typeof fetch;

    await expect(startBillingFlow('/api/billing/portal')).rejects.toThrow(/billing isn’t enabled/i);
  });

  it('reports a friendly message when the request itself fails', async () => {
    globalThis.fetch = vi.fn().mockRejectedValue(new TypeError('network down')) as unknown as typeof fetch;

    await expect(startBillingFlow('/api/billing/portal')).rejects.toThrow(/couldn't reach/i);
    expect(assign).not.toHaveBeenCalled();
  });
});
