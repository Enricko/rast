-- AI Agent: files people add to their messages.
--
-- The bytes live under <data_dir>/ai-files/<user id>/<id>; this lists them.
-- A file belongs to its account until it is sent, then to the chat it was
-- sent in (and goes with that chat). Files never sent are removed after a
-- day.
CREATE TABLE ai_files (
    id              TEXT    PRIMARY KEY,
    user_id         INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    conversation_id TEXT    REFERENCES ai_conversations(id) ON DELETE CASCADE,
    name            TEXT    NOT NULL,
    mime            TEXT    NOT NULL,
    kind            TEXT    NOT NULL CHECK (kind IN ('image', 'pdf', 'text')),
    size_bytes      INTEGER NOT NULL,
    created_at      INTEGER NOT NULL
);
CREATE INDEX ai_files_user ON ai_files(user_id, created_at);
CREATE INDEX ai_files_conversation ON ai_files(conversation_id);
