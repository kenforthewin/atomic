/// Small timezone helpers shared by the report editor and the existing
/// per-DB timezone setting. Centralizes the fallback list + the
/// progressive-enhancement check for `Intl.supportedValuesOf` so two
/// callers don't drift.

const FALLBACK_TIMEZONES = [
  'UTC',
  'America/New_York',
  'America/Chicago',
  'America/Denver',
  'America/Los_Angeles',
  'Europe/London',
  'Europe/Paris',
  'Asia/Tokyo',
  'Australia/Sydney',
];

/// The browser's resolved IANA tz. Falls back to UTC if the resolver
/// returns an empty string (very old environments).
export function getBrowserTimeZone(): string {
  return Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC';
}

/// All IANA timezones the runtime knows about, or a small curated list
/// when `Intl.supportedValuesOf` is unavailable (Safari < 17, some
/// embedded webviews).
export function getSupportedTimeZones(): string[] {
  const supportedValuesOf = (Intl as unknown as {
    supportedValuesOf?: (key: string) => string[];
  }).supportedValuesOf;
  if (typeof supportedValuesOf === 'function') {
    try {
      return supportedValuesOf('timeZone');
    } catch {
      return FALLBACK_TIMEZONES;
    }
  }
  return FALLBACK_TIMEZONES;
}
