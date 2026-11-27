-- AI Agent: providers an account signs in to with its own plan (ChatGPT's
-- Codex) instead of an API key.
--
-- owner_id is the account that signed in: only it may chat with the
-- provider (a plan is one person's). sign_in holds the sign-in's tokens
-- ({"accessToken", "refreshToken", "expiresAt", "refreshedAt", "accountId",
-- "email", "plan"}), encrypted like API keys; "" when not signed in.
ALTER TABLE ai_providers ADD COLUMN owner_id INTEGER REFERENCES users(id) ON DELETE SET NULL;
ALTER TABLE ai_providers ADD COLUMN sign_in TEXT NOT NULL DEFAULT '';
