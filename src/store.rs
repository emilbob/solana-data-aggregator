use crate::db::{InMemoryDatabase, TransactionData, Transfer};
use log::warn;
use sqlx::postgres::{PgPool, PgPoolOptions, PgRow};
use sqlx::Row;

/// Storage backend. In-memory + file (the default, zero-setup) or Postgres (when
/// `DATABASE_URL` is set). The rest of the app talks only to this enum, so the
/// aggregator and API don't care which backend is live.
pub enum Store {
    Memory(InMemoryDatabase),
    Postgres(PgStore),
}

impl Store {
    /// In-memory store persisted to `file_path`.
    pub fn memory(file_path: impl Into<String>) -> Self {
        Store::Memory(InMemoryDatabase::new(file_path.into()))
    }

    /// Connects to Postgres and runs migrations. Returns an error message on failure.
    pub async fn connect_postgres(url: &str) -> Result<Self, String> {
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(url)
            .await
            .map_err(|e| format!("connecting to Postgres: {e}"))?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| format!("running migrations: {e}"))?;
        Ok(Store::Postgres(PgStore { pool }))
    }

    pub async fn add_transaction(&self, account: &str, tx: TransactionData) {
        match self {
            Store::Memory(m) => m.add_transaction(account, tx).await,
            Store::Postgres(p) => p.add_transaction(account, tx).await,
        }
    }

    pub async fn get_transactions(&self, account: &str) -> Vec<TransactionData> {
        match self {
            Store::Memory(m) => m.get_transactions(account).await,
            Store::Postgres(p) => p.get_transactions(account).await,
        }
    }

    pub async fn get_transaction_by_signature(&self, signature: &str) -> Option<TransactionData> {
        match self {
            Store::Memory(m) => m.get_transaction_by_signature(signature).await,
            Store::Postgres(p) => p.get_transaction_by_signature(signature).await,
        }
    }

    /// Loads any persisted data at startup. No-op for Postgres (already durable).
    pub async fn load(&self) {
        if let Store::Memory(m) = self {
            m.load_from_file().await;
        }
    }

    /// Total number of stored transactions across all accounts.
    pub async fn count(&self) -> u64 {
        match self {
            Store::Memory(m) => m.count().await,
            Store::Postgres(p) => p.count().await,
        }
    }
}

/// Postgres-backed store. Idempotent by `(account, signature)`.
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    async fn add_transaction(&self, account: &str, tx: TransactionData) {
        let (src, dst, lamports) = match &tx.transfer {
            Some(t) => (
                Some(t.source.clone()),
                Some(t.destination.clone()),
                Some(t.lamports as i64),
            ),
            None => (None, None, None),
        };
        let res = sqlx::query(
            "INSERT INTO transactions \
             (account, signature, slot, block_time, fee, fee_payer, success, tx_type, programs, \
              transfer_source, transfer_destination, transfer_lamports) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) \
             ON CONFLICT (account, signature) DO NOTHING",
        )
        .bind(account)
        .bind(&tx.signature)
        .bind(tx.slot as i64)
        .bind(tx.timestamp as i64)
        .bind(tx.fee as i64)
        .bind(&tx.fee_payer)
        .bind(tx.success)
        .bind(&tx.tx_type)
        .bind(&tx.programs)
        .bind(src)
        .bind(dst)
        .bind(lamports)
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            warn!(
                "Failed to persist transaction {} to Postgres: {e}",
                tx.signature
            );
        }
    }

    async fn get_transactions(&self, account: &str) -> Vec<TransactionData> {
        match sqlx::query("SELECT * FROM transactions WHERE account = $1 ORDER BY slot DESC")
            .bind(account)
            .fetch_all(&self.pool)
            .await
        {
            Ok(rows) => rows.iter().map(row_to_tx).collect(),
            Err(e) => {
                warn!("Postgres query for {account} failed: {e}");
                Vec::new()
            }
        }
    }

    async fn get_transaction_by_signature(&self, signature: &str) -> Option<TransactionData> {
        let row = sqlx::query("SELECT * FROM transactions WHERE signature = $1 LIMIT 1")
            .bind(signature)
            .fetch_optional(&self.pool)
            .await
            .ok()??;
        Some(row_to_tx(&row))
    }

    async fn count(&self) -> u64 {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM transactions")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0) as u64
    }
}

/// Reconstructs a `TransactionData` from a row, rebuilding the optional transfer
/// from its flattened columns.
fn row_to_tx(row: &PgRow) -> TransactionData {
    let src: Option<String> = row.get("transfer_source");
    let dst: Option<String> = row.get("transfer_destination");
    let lamports: Option<i64> = row.get("transfer_lamports");
    let transfer = match (src, dst, lamports) {
        (Some(source), Some(destination), Some(l)) => Some(Transfer {
            source,
            destination,
            lamports: l as u64,
        }),
        _ => None,
    };
    TransactionData {
        signature: row.get("signature"),
        slot: row.get::<i64, _>("slot") as u64,
        timestamp: row.get::<i64, _>("block_time") as u64,
        fee: row.get::<i64, _>("fee") as u64,
        fee_payer: row.get("fee_payer"),
        success: row.get("success"),
        tx_type: row.get("tx_type"),
        programs: row.get("programs"),
        transfer,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Round-trips a transaction through Postgres: fields preserved, transfer
    /// reconstructed, and idempotent by (account, signature). Skips (passing)
    /// when `DATABASE_URL` isn't set, so it's a no-op in CI without a database
    /// and a real integration test locally (`docker compose up -d` first).
    #[tokio::test]
    async fn pg_round_trip_and_idempotent() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("DATABASE_URL not set — skipping Postgres integration test");
            return;
        };
        let store = Store::connect_postgres(&url)
            .await
            .expect("connect + migrate");

        // Unique account per run so concurrent/repeat runs don't collide.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let account = format!("test_acct_{nanos}");

        let tx = TransactionData {
            signature: format!("test_sig_{nanos}"),
            slot: 12345,
            timestamp: 1_700_000_000,
            fee: 5000,
            fee_payer: "PayerPubkey".to_string(),
            success: true,
            tx_type: "transfer".to_string(),
            programs: vec!["system".to_string()],
            transfer: Some(Transfer {
                source: "SourcePubkey".to_string(),
                destination: "DestPubkey".to_string(),
                lamports: 250_000_000,
            }),
        };

        // Insert twice — idempotent by (account, signature).
        store.add_transaction(&account, tx.clone()).await;
        store.add_transaction(&account, tx.clone()).await;

        let rows = store.get_transactions(&account).await;
        assert_eq!(rows.len(), 1, "duplicate insert must be a no-op");
        assert_eq!(rows[0], tx, "all fields (incl. transfer) must round-trip");

        // Lookup by signature works too.
        let by_sig = store.get_transaction_by_signature(&tx.signature).await;
        assert_eq!(by_sig.as_ref(), Some(&tx));
    }
}
