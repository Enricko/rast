-- AI Agent: prices from the providers' own model lists, and cached input.
--
-- ai_providers.prices stays what administrators set; list_prices is what
-- the provider's model list gives ({"model": {"input", "output", "cached",
-- "cacheWrite"}}, USD per million tokens), refreshed daily.
ALTER TABLE ai_providers ADD COLUMN list_prices TEXT NOT NULL DEFAULT '{}'
    CHECK (json_valid(list_prices));
ALTER TABLE ai_providers ADD COLUMN prices_checked_at INTEGER;

-- Parts of input_tokens read back from the provider's prompt cache and
-- written to it (each priced on its own).
ALTER TABLE ai_usage ADD COLUMN cache_read_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE ai_usage ADD COLUMN cache_write_tokens INTEGER NOT NULL DEFAULT 0;
