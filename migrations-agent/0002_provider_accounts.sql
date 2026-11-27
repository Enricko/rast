-- One provider, several accounts. An account is a provider row
-- whose pool_id names the provider it belongs to (the provider itself is
-- its first account). Replies take an account by the provider's rotation
-- ({"mode": "fill" | "round", "sticky": n}); an account that hits its limit
-- rests until resting_until. quota keeps the limits the provider last
-- reported for the account; ai_usage.account_id says which account a reply
-- used (provider_id stays the provider, for prices and statistics).
ALTER TABLE ai_providers ADD COLUMN pool_id TEXT;
ALTER TABLE ai_providers ADD COLUMN position INTEGER NOT NULL DEFAULT 0;
ALTER TABLE ai_providers ADD COLUMN rotation TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(rotation));
ALTER TABLE ai_providers ADD COLUMN resting_until INTEGER;
ALTER TABLE ai_providers ADD COLUMN resting_reason TEXT NOT NULL DEFAULT '';
ALTER TABLE ai_providers ADD COLUMN quota TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(quota));
-- An account an administrator set aside (enabled stays the provider's switch).
ALTER TABLE ai_providers ADD COLUMN paused INTEGER NOT NULL DEFAULT 0;
CREATE INDEX ai_providers_pool ON ai_providers(pool_id);
ALTER TABLE ai_usage ADD COLUMN account_id TEXT;
CREATE INDEX ai_usage_account_time ON ai_usage(account_id, created_at);

-- Sign-ins to one plan (same kind, address and owner) become accounts of
-- the oldest of them.
UPDATE ai_providers
SET pool_id = (
    SELECT first.id FROM ai_providers AS first
    WHERE first.kind = ai_providers.kind
      AND first.base_url = ai_providers.base_url
      AND first.owner_id IS ai_providers.owner_id
      AND first.sign_in <> ''
    ORDER BY first.created_at, first.id
    LIMIT 1
)
WHERE sign_in <> '';
UPDATE ai_providers SET pool_id = NULL WHERE pool_id = id;
UPDATE ai_providers
SET position = (
    SELECT COUNT(*) FROM ai_providers AS earlier
    WHERE earlier.pool_id = ai_providers.pool_id
      AND (earlier.created_at < ai_providers.created_at
           OR (earlier.created_at = ai_providers.created_at AND earlier.id < ai_providers.id))
) + 1
WHERE pool_id IS NOT NULL;

-- Chats, agents and limits that named an account now name its provider.
UPDATE ai_conversations
SET provider_id = (SELECT pool_id FROM ai_providers WHERE id = ai_conversations.provider_id)
WHERE provider_id IN (SELECT id FROM ai_providers WHERE pool_id IS NOT NULL);
UPDATE ai_agents
SET provider_id = (SELECT pool_id FROM ai_providers WHERE id = ai_agents.provider_id)
WHERE provider_id IN (SELECT id FROM ai_providers WHERE pool_id IS NOT NULL);
UPDATE ai_limits
SET providers = (
    SELECT json_group_array(DISTINCT COALESCE(p.pool_id, j.value))
    FROM json_each(ai_limits.providers) AS j
    LEFT JOIN ai_providers AS p ON p.id = j.value
)
WHERE providers IS NOT NULL AND json_array_length(providers) > 0;
-- Usage an account recorded before stays with it.
UPDATE ai_usage SET account_id = provider_id WHERE provider_id IS NOT NULL;
UPDATE ai_usage
SET provider_id = (SELECT pool_id FROM ai_providers WHERE id = ai_usage.provider_id)
WHERE provider_id IN (SELECT id FROM ai_providers WHERE pool_id IS NOT NULL);
