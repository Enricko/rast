-- AI Agent (schema v11): RAST's own agent harness.
--
-- ai_providers: model accounts administrators set up (Anthropic, OpenAI,
--   DeepSeek, OpenRouter, Ollama or any OpenAI-compatible endpoint). Several
--   accounts of one kind are fine. The API key is encrypted with the server
--   secret (secrets.rs) and never returned by the API.
-- ai_skills: instructions an agent can carry (written by administrators).
-- ai_agents: the agents ("profiles") people chat with: personality/system
--   prompt, provider and model, skills.
-- ai_conversations / ai_messages: each account's own conversations
--   (administrators can read them). Message content is a JSON array of
--   blocks ({type: "text" | "reasoning", text}).
-- ai_usage: tokens spent per account and call, kept when a conversation is
--   deleted (allowances count what was used).
-- ai_limits: which providers an account may use and how many tokens per day
--   and month; subject 'default', 'group:<id>' or 'user:<id>'. NULL = no limit
--   (providers: all).

CREATE TABLE ai_providers (
    id          TEXT    PRIMARY KEY,
    kind        TEXT    NOT NULL,
    name        TEXT    NOT NULL,
    base_url    TEXT    NOT NULL DEFAULT '',
    api_key     TEXT    NOT NULL DEFAULT '',
    models      TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(models)),
    enabled     INTEGER NOT NULL DEFAULT 1,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE ai_skills (
    id           TEXT    PRIMARY KEY,
    name         TEXT    NOT NULL,
    description  TEXT    NOT NULL DEFAULT '',
    instructions TEXT    NOT NULL DEFAULT '',
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL
);

CREATE TABLE ai_agents (
    id                TEXT    PRIMARY KEY,
    name              TEXT    NOT NULL,
    description       TEXT    NOT NULL DEFAULT '',
    system_prompt     TEXT    NOT NULL DEFAULT '',
    provider_id       TEXT    REFERENCES ai_providers(id) ON DELETE SET NULL,
    model             TEXT    NOT NULL DEFAULT '',
    skills            TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(skills)),
    temperature       REAL,
    max_output_tokens INTEGER,
    enabled           INTEGER NOT NULL DEFAULT 1,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL
);

CREATE TABLE ai_conversations (
    id          TEXT    PRIMARY KEY,
    user_id     INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    agent_id    TEXT    REFERENCES ai_agents(id) ON DELETE SET NULL,
    provider_id TEXT    REFERENCES ai_providers(id) ON DELETE SET NULL,
    model       TEXT    NOT NULL DEFAULT '',
    title       TEXT    NOT NULL DEFAULT '',
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE INDEX ai_conversations_user ON ai_conversations(user_id, updated_at DESC);
CREATE INDEX ai_conversations_updated ON ai_conversations(updated_at DESC);

CREATE TABLE ai_messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id TEXT    NOT NULL REFERENCES ai_conversations(id) ON DELETE CASCADE,
    role            TEXT    NOT NULL CHECK (role IN ('user', 'assistant')),
    content         TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(content)),
    provider_id     TEXT,
    model           TEXT    NOT NULL DEFAULT '',
    input_tokens    INTEGER NOT NULL DEFAULT 0,
    output_tokens   INTEGER NOT NULL DEFAULT 0,
    status          TEXT    NOT NULL DEFAULT 'done'
                            CHECK (status IN ('streaming', 'done', 'stopped', 'error')),
    error           TEXT    NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL,
    finished_at     INTEGER
);
CREATE INDEX ai_messages_conversation ON ai_messages(conversation_id, id);

CREATE TABLE ai_usage (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id       INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider_id   TEXT,
    model         TEXT    NOT NULL DEFAULT '',
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL
);
CREATE INDEX ai_usage_user_time ON ai_usage(user_id, created_at);

CREATE TABLE ai_limits (
    subject        TEXT    PRIMARY KEY,
    providers      TEXT    CHECK (providers IS NULL OR json_valid(providers)),
    daily_tokens   INTEGER,
    monthly_tokens INTEGER,
    updated_at     INTEGER NOT NULL
);
