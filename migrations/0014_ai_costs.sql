-- AI Agent: weekly token limits, spending limits and model prices.
--
-- Money is kept in micro-dollars (1 USD = 1 000 000) as whole numbers.

-- Prices per model, in USD per million tokens: {"model": [input, output]}.
ALTER TABLE ai_providers ADD COLUMN prices TEXT NOT NULL DEFAULT '{}'
    CHECK (json_valid(prices));

-- What each reply cost when it was made (prices may change later).
ALTER TABLE ai_usage ADD COLUMN cost INTEGER NOT NULL DEFAULT 0;

ALTER TABLE ai_limits ADD COLUMN weekly_tokens INTEGER;
ALTER TABLE ai_limits ADD COLUMN daily_cost INTEGER;
ALTER TABLE ai_limits ADD COLUMN weekly_cost INTEGER;
ALTER TABLE ai_limits ADD COLUMN monthly_cost INTEGER;
