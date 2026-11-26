-- AI Agent tools (schema v12).
--
-- ai_agents.tools: the tools an agent may use (JSON array of tool names;
--   NULL = every tool the chatting account may use).
-- ai_conversations.allowed_tools: tools the owner let run without asking in
--   this chat (JSON array; never the shell tool).
-- ai_messages.status gains 'waiting': the reply asked to use a tool and
--   waits for the owner's answer. Message content blocks may now also be
--   {type: "tool", id, name, input, status, output?, error?}.

ALTER TABLE ai_agents ADD COLUMN tools TEXT CHECK (tools IS NULL OR json_valid(tools));

ALTER TABLE ai_conversations ADD COLUMN allowed_tools TEXT NOT NULL DEFAULT '[]'
    CHECK (json_valid(allowed_tools));

CREATE TABLE ai_messages_new (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT    NOT NULL REFERENCES ai_conversations(id) ON DELETE CASCADE,
    role            TEXT    NOT NULL CHECK (role IN ('user', 'assistant')),
    content         TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(content)),
    provider_id     TEXT,
    model           TEXT    NOT NULL DEFAULT '',
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    status          TEXT    NOT NULL DEFAULT 'done'
                            CHECK (status IN ('streaming', 'waiting', 'done', 'stopped', 'error')),
    error           TEXT    NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL,
    finished_at     INTEGER
);
INSERT INTO ai_messages_new
    SELECT id, conversation_id, role, content, provider_id, model, input_tokens, output_tokens,
           status, error, created_at, finished_at
    FROM ai_messages;
DROP TABLE ai_messages;
ALTER TABLE ai_messages_new RENAME TO ai_messages;
CREATE INDEX ai_messages_conversation ON ai_messages(conversation_id, id);
