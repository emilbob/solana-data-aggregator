-- Transactions indexed by monitored account. One row per (account, signature).
CREATE TABLE IF NOT EXISTS transactions (
    account              TEXT    NOT NULL,
    signature            TEXT    NOT NULL,
    slot                 BIGINT  NOT NULL,
    block_time           BIGINT  NOT NULL,
    fee                  BIGINT  NOT NULL,
    fee_payer            TEXT    NOT NULL,
    success              BOOLEAN NOT NULL,
    tx_type              TEXT    NOT NULL,
    programs             TEXT[]  NOT NULL DEFAULT '{}',
    -- Native SOL transfer, flattened so it can be filtered/summed in SQL.
    -- All three are NULL together when the tx isn't a transfer.
    transfer_source      TEXT,
    transfer_destination TEXT,
    transfer_lamports    BIGINT,
    PRIMARY KEY (account, signature)
);

-- Query path: an account's transactions, newest first.
CREATE INDEX IF NOT EXISTS idx_transactions_account_slot
    ON transactions (account, slot DESC);

-- Lookup a single transaction by signature across accounts.
CREATE INDEX IF NOT EXISTS idx_transactions_signature
    ON transactions (signature);

-- Date-range / day filtering without a full scan.
CREATE INDEX IF NOT EXISTS idx_transactions_account_block_time
    ON transactions (account, block_time);
