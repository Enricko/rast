-- AI Agent's own database (agent.db), for its Package Center app. The same
-- tables as RAST's migrations 0011–0019 made in rast.db, at their latest
-- shape. Accounts live in RAST: user ids are plain numbers here, and the
-- app removes a removed account's rows itself.

CREATE TABLE settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE ai_providers (
    id                TEXT    PRIMARY KEY,
    kind              TEXT    NOT NULL,
    name              TEXT    NOT NULL,
    base_url          TEXT    NOT NULL DEFAULT '',
    api_key           TEXT    NOT NULL DEFAULT '',
    models            TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(models)),
    enabled           INTEGER NOT NULL DEFAULT 1,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    prices            TEXT    NOT NULL DEFAULT '{}' CHECK (json_valid(prices)),
    list_prices       TEXT    NOT NULL DEFAULT '{}' CHECK (json_valid(list_prices)),
    prices_checked_at INTEGER,
    owner_id          INTEGER,
    sign_in           TEXT    NOT NULL DEFAULT ''
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
    updated_at        INTEGER NOT NULL,
    tools             TEXT    CHECK (tools IS NULL OR json_valid(tools))
);

CREATE TABLE ai_conversations (
    id            TEXT    PRIMARY KEY,
    user_id       INTEGER NOT NULL,
    agent_id      TEXT    REFERENCES ai_agents(id) ON DELETE SET NULL,
    provider_id   TEXT    REFERENCES ai_providers(id) ON DELETE SET NULL,
    model         TEXT    NOT NULL DEFAULT '',
    title         TEXT    NOT NULL DEFAULT '',
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    allowed_tools TEXT    NOT NULL DEFAULT '[]' CHECK (json_valid(allowed_tools)),
    effort        TEXT    NOT NULL DEFAULT ''
                          CHECK (effort IN ('', 'low', 'medium', 'high', 'max')),
    pinned_at     INTEGER
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
                            CHECK (status IN ('streaming', 'waiting', 'done', 'stopped', 'error')),
    error           TEXT    NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL,
    finished_at     INTEGER,
    estimated_cost  INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX ai_messages_conversation ON ai_messages(conversation_id, id);

CREATE TABLE ai_usage (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id            INTEGER NOT NULL,
    provider_id        TEXT,
    model              TEXT    NOT NULL DEFAULT '',
    input_tokens       INTEGER NOT NULL DEFAULT 0,
    output_tokens      INTEGER NOT NULL DEFAULT 0,
    created_at         INTEGER NOT NULL,
    cost               INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    estimated_cost     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX ai_usage_user_time ON ai_usage(user_id, created_at);

CREATE TABLE ai_limits (
    subject        TEXT    PRIMARY KEY,
    providers      TEXT    CHECK (providers IS NULL OR json_valid(providers)),
    daily_tokens   INTEGER,
    monthly_tokens INTEGER,
    updated_at     INTEGER NOT NULL,
    weekly_tokens  INTEGER,
    daily_cost     INTEGER,
    weekly_cost    INTEGER,
    monthly_cost   INTEGER
);

CREATE TABLE ai_files (
    id              TEXT    PRIMARY KEY,
    user_id         INTEGER NOT NULL,
    conversation_id TEXT    REFERENCES ai_conversations(id) ON DELETE CASCADE,
    name            TEXT    NOT NULL,
    mime            TEXT    NOT NULL,
    kind            TEXT    NOT NULL CHECK (kind IN ('image', 'pdf', 'text')),
    size_bytes      INTEGER NOT NULL,
    created_at      INTEGER NOT NULL
);
CREATE INDEX ai_files_user ON ai_files(user_id, created_at);
CREATE INDEX ai_files_conversation ON ai_files(conversation_id);
