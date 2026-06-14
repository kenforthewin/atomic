/**
 * Client-side format checks that mirror the server's `email_format_ok` /
 * `subdomain_format_ok`. These are UX-only: the server re-validates and owns
 * the authoritative answer (a subdomain race is only resolved at the accounts
 * UNIQUE constraint). They exist to give instant inline feedback before a
 * round-trip.
 */

/** Subdomain rule: 3–32 chars of `[a-z0-9-]`. */
export const SUBDOMAIN_PATTERN = /^[a-z0-9-]{3,32}$/;

/** A permissive single-`@` email shape — the server is the real gate. */
const EMAIL_PATTERN = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

export function isEmailFormatOk(email: string): boolean {
  return EMAIL_PATTERN.test(email.trim());
}

export function isSubdomainFormatOk(subdomain: string): boolean {
  return SUBDOMAIN_PATTERN.test(subdomain);
}

/**
 * Normalize keystrokes toward a valid subdomain as the user types: lowercase,
 * strip anything outside `[a-z0-9-]`, and clamp length. Leading/trailing
 * hyphens are allowed mid-edit (the format check still rejects an all-hyphen or
 * too-short value) so the field doesn't fight the user.
 */
export function normalizeSubdomainInput(raw: string): string {
  return raw
    .toLowerCase()
    .replace(/[^a-z0-9-]/g, '')
    .slice(0, 32);
}
