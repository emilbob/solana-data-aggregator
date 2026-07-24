-- Add a network dimension so the same account can be tracked on multiple
-- clusters (mainnet/devnet/testnet) independently. The primary key becomes
-- (network, account, signature).
ALTER TABLE transactions
    ADD COLUMN IF NOT EXISTS network TEXT NOT NULL DEFAULT 'mainnet';

ALTER TABLE transactions DROP CONSTRAINT IF EXISTS transactions_pkey;
ALTER TABLE transactions ADD PRIMARY KEY (network, account, signature);

-- Query path: an account's transactions on a network, newest first.
CREATE INDEX IF NOT EXISTS idx_transactions_network_account_slot
    ON transactions (network, account, slot DESC);
