/**
 * The curated managed-model catalogue, mirrored from the server's
 * `crate::curated_models`. The server is the authority — these constants drive
 * the managed model picker's options and the dimension hint; an out-of-list
 * choice is rejected server-side (`model_not_curated`).
 */

/** The platform-pinned embedding model for managed keys (server:
 * `MANAGED_EMBEDDING_MODEL`). Fixed — changing it would invalidate every
 * stored vector, so the managed picker shows it read-only. */
export const MANAGED_EMBEDDING_MODEL = 'openai/text-embedding-3-small';

/** The platform-pinned embedding dimension (server:
 * `PINNED_EMBEDDING_DIMENSION`). A BYOK config whose effective dimension
 * differs is rejected server-side; the BYOK form surfaces that. */
export const PINNED_EMBEDDING_DIMENSION = 1536;

/**
 * The curated managed LLM list (server: `MANAGED_LLM_MODELS`). The first is
 * the signup default.
 *
 * TODO(DASH-2): this is a hand-maintained mirror of the server's
 * `crate::curated_models::MANAGED_LLM_MODELS` with no drift guard. The server
 * is the authority — an out-of-list choice is rejected with `model_not_curated`
 * — so a stale entry here surfaces a curated model the server has dropped (its
 * save fails) or hides a newly-curated one. There is no API that returns this
 * list today; close the drift by either exposing the curated catalogue on the
 * overview/provider response and rendering the picker from it, or adding a CI
 * assertion that pins these ids against the server constant. Until then, keep
 * the two lists in lockstep when editing either side.
 */
export const MANAGED_LLM_MODELS: ReadonlyArray<{ id: string; label: string }> = [
  { id: 'openai/gpt-4o-mini', label: 'GPT-4o mini' },
  { id: 'anthropic/claude-3.5-haiku', label: 'Claude 3.5 Haiku' },
  { id: 'google/gemini-2.0-flash-001', label: 'Gemini 2.0 Flash' },
];

/** Human label for a managed LLM id, falling back to the raw id. */
export function managedLlmLabel(id: string): string {
  return MANAGED_LLM_MODELS.find((m) => m.id === id)?.label ?? id;
}
