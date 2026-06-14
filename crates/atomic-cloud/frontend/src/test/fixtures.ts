import type { AccountOverview } from '../lib/api';

/**
 * A ready account overview for tests, with sane defaults. Pass a partial to
 * override any field (e.g. `overview({ billing_state: 'read_only' })`).
 */
export function overview(patch: Partial<AccountOverview> = {}): AccountOverview {
  return {
    subdomain: 'alpha',
    email: 'alpha@example.com',
    plan: { id: 'pro', name: 'Pro' },
    billing_state: 'active',
    billing_configured: true,
    trial_ends_at: null,
    usage: {
      atoms_used: 3,
      atom_limit: null,
      kb_count: 1,
      kb_limit: null,
      ai_credits_monthly_cents: 50,
    },
    provider: {
      configured: true,
      origin: 'managed',
      provider: 'openrouter',
      model_config: { embedding_model: 'openai/text-embedding-3-small' },
      last_validated_at: null,
      last_validation_error: null,
    },
    mcp_url: 'https://alpha.atomic.cloud/mcp',
    ...patch,
  };
}
