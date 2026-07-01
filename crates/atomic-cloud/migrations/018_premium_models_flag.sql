-- Premium (paid) plans unlock the fuller agentic model list for wiki/chat/
-- reports (crate::curated_models::PRO_AGENTIC_MODELS vs FREE_AGENTIC_MODELS).
-- The tenant provider-models route reads this flag per write to pick which
-- list a model selection is validated against. Free stays '{}' (no flag).
UPDATE plans SET feature_flags = '{"premium_models": true}'::jsonb WHERE id = 'pro';

-- Record this migration in the version table (the runner reads MAX(version)).
INSERT INTO schema_version (version) VALUES (18);
