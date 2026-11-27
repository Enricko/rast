-- AI Agent: chats pinned to the top of their owner's list (when, NULL not
-- pinned).
ALTER TABLE ai_conversations ADD COLUMN pinned_at INTEGER;
