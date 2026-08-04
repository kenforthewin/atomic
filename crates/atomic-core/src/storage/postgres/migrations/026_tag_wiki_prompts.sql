-- Migration 026: a tag can override the wiki prompts used for its own article.
--
-- NULL means "no override" — the resolver in
-- `AtomicCore::build_wiki_strategy_context` then falls through to the global
-- setting and finally the built-in prompt, so existing tags keep behaving
-- exactly as they do today.

ALTER TABLE tags ADD COLUMN IF NOT EXISTS wiki_generation_prompt TEXT;
ALTER TABLE tags ADD COLUMN IF NOT EXISTS wiki_update_prompt TEXT;

INSERT INTO schema_version (version) VALUES (26);
