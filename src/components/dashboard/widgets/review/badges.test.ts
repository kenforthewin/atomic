import { describe, it, expect } from 'vitest';
import { sourceTrust, relativeAge } from './badges';

describe('sourceTrust', () => {
  it('returns official for docs.* URL', () => {
    const result = sourceTrust('https://docs.example.com/guide');
    expect(result.tone).toBe('official');
    expect(result.label).toBe('docs.example.com');
  });

  it('returns manual for undefined', () => {
    const result = sourceTrust(undefined);
    expect(result.tone).toBe('manual');
    expect(result.label).toBe('manual');
  });

  it('returns personal for arbitrary host', () => {
    const result = sourceTrust('https://myblog.io/post/1');
    expect(result.tone).toBe('personal');
    expect(result.label).toBe('myblog.io');
  });

  it('strips www. prefix', () => {
    const result = sourceTrust('https://www.example.com/page');
    expect(result.label).toBe('example.com');
  });

  it('returns official for developer.* URL', () => {
    const result = sourceTrust('https://developer.mozilla.org/en/docs');
    expect(result.tone).toBe('official');
  });
});

describe('relativeAge', () => {
  it('returns today for now', () => {
    expect(relativeAge(new Date().toISOString())).toBe('today');
  });

  it('returns Xd ago for days', () => {
    const threeDaysAgo = new Date(Date.now() - 3 * 86_400_000).toISOString();
    expect(relativeAge(threeDaysAgo)).toBe('3d ago');
  });

  it('returns Xw ago for weeks', () => {
    const twoWeeksAgo = new Date(Date.now() - 14 * 86_400_000).toISOString();
    expect(relativeAge(twoWeeksAgo)).toBe('2w ago');
  });

  it('returns Xmo ago for months', () => {
    const twoMonthsAgo = new Date(Date.now() - 60 * 86_400_000).toISOString();
    expect(relativeAge(twoMonthsAgo)).toBe('2mo ago');
  });

  it('returns Xy ago for years', () => {
    const twoYearsAgo = new Date(Date.now() - 730 * 86_400_000).toISOString();
    expect(relativeAge(twoYearsAgo)).toBe('2y ago');
  });

  it('returns null for undefined', () => {
    expect(relativeAge(undefined)).toBeNull();
  });
});
