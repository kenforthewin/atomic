import { describe, it, expect, vi, afterEach } from 'vitest';
import { billingDescriptor, billingNotice } from './billing';
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
