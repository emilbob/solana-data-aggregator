-- SPL token balance changes, stored as a JSON array (text) alongside each tx.
ALTER TABLE transactions
    ADD COLUMN IF NOT EXISTS token_changes TEXT NOT NULL DEFAULT '[]';
