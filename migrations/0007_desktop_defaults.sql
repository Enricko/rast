-- Desktop defaults per account (schema v7).
--
-- widgets_customized: whether the account ever chose its own widgets, so the
-- desktop can offer a default set to accounts that never did, while an
-- account that removed every widget keeps an empty desktop. Rows that already
-- hold widgets (saved with 0.4.0) count as customised.
--
-- taskbar_pins: the account's pinned taskbar applications, in order (JSON
-- array of application ids, validated by the appearance API). Every account
-- starts with File Station, Resource Monitor and Control Panel; pins follow
-- the account to any browser.

ALTER TABLE user_appearance ADD COLUMN widgets_customized INTEGER NOT NULL DEFAULT 0
    CHECK (widgets_customized IN (0, 1));

UPDATE user_appearance SET widgets_customized = 1 WHERE widgets <> '[]';

ALTER TABLE user_appearance ADD COLUMN taskbar_pins TEXT NOT NULL
    DEFAULT '["files","monitor","controlpanel"]'
    CHECK (json_valid(taskbar_pins) AND json_type(taskbar_pins) = 'array');
