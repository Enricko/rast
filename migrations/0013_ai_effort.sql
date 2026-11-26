-- AI Agent effort (schema v13).
--
-- ai_conversations.effort: how hard the model thinks in this chat
--   ('' = the provider's default, 'low', 'medium', 'high' or 'max').

ALTER TABLE ai_conversations ADD COLUMN effort TEXT NOT NULL DEFAULT ''
    CHECK (effort IN ('', 'low', 'medium', 'high', 'max'));
