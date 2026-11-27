-- AI Agent: what replies are worth at API prices, kept apart from what is
-- counted toward spending limits.
--
-- ai_usage.cost stays what limits count (a signed-in plan counts nothing);
-- estimated_cost is the same tokens at the model's API price (for a plan:
-- the price set, the list's or the built-in one), in micro-dollars. Rows
-- written before this keep their counted cost as the estimate.
ALTER TABLE ai_usage ADD COLUMN estimated_cost INTEGER NOT NULL DEFAULT 0;
UPDATE ai_usage SET estimated_cost = cost;

-- Each reply's estimate, added up over its steps.
ALTER TABLE ai_messages ADD COLUMN estimated_cost INTEGER NOT NULL DEFAULT 0;
