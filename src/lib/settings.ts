/**
 * Setting source classification — mirrors `atomic_core::settings::SettingSource`.
 *
 *  - `workspace`         — key is workspace-only (theme, font, credentials,
 *                          machine URLs); lives in registry.db, never overridable.
 *  - `workspace_default` — overridable key, currently using the value stored
 *                          in registry.db. The active DB has no override.
 *  - `override`          — overridable key, the active DB has its own row in
 *                          its per-DB settings table.
 *  - `builtin_default`   — no row in registry or per-DB; value is the constant
 *                          baked into the binary.
 */
export type SettingSource =
  | 'workspace'
  | 'workspace_default'
  | 'override'
  | 'builtin_default';

export interface SettingValue {
  value: string;
  source: SettingSource;
}

/**
 * Keys that are workspace-only — lives in registry.db, cannot be overridden
 * per-DB. Mirrors `atomic_core::settings::WORKSPACE_ONLY_KEYS`. Kept in sync
 * by hand; the override UI uses this to suppress override affordances on
 * fields that aren't overridable in the first place.
 */
export const WORKSPACE_ONLY_KEYS: readonly string[] = [
  'theme',
  'font',
  'timezone',
  'openrouter_api_key',
  'openai_compat_api_key',
  'orcarouter_api_key',
  'ollama_host',
  'openai_compat_base_url',
];

export function isWorkspaceOnly(key: string): boolean {
  return WORKSPACE_ONLY_KEYS.includes(key);
}
