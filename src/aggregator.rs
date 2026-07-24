use crate::db::{InMemoryDatabase, TransactionData};
use futures::stream::StreamExt;
use log::{info, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_client::rpc_response::RpcConfirmedTransactionStatusWithSignature;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status::{
    EncodedTransaction, UiMessage, UiTransaction, UiTransactionEncoding,
};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::time::{error::Elapsed, timeout, Duration};

/// How many per-signature detail fetches to run concurrently within one cycle.
const FETCH_CONCURRENCY: usize = 8;

/// Custom error type for the `Aggregator` struct, encapsulating various errors
/// that can occur while interacting with the Solana blockchain.
#[derive(Debug, Error)]
pub enum AggregatorError {
    /// Indicates an invalid public key format.
    #[error("Invalid public key format")]
    InvalidPublicKey,

    /// Error that occurs when fetching signatures from the Solana blockchain.
    #[error("Failed to fetch signatures: {0}")]
    FetchSignaturesError(#[source] solana_client::client_error::ClientError),

    /// Error that occurs when fetching transaction details from the Solana blockchain.
    #[error("Failed to fetch transaction details: {0}")]
    FetchTransactionError(#[source] solana_client::client_error::ClientError),

    /// Indicates that an operation has timed out.
    #[error("Operation timed out")]
    Elapsed(#[from] Elapsed),
}

/// Struct that handles fetching transactions from the Solana blockchain and storing
/// them in an in-memory database.
pub struct Aggregator {
    client: RpcClient,         // Solana RPC client used to interact with the blockchain
    db: Arc<InMemoryDatabase>, // In-memory database for storing transactions
    fetch_limit: usize,        // Max signatures to pull per cycle (bounds the per-cycle work)
    /// Newest signature ingested so far. Passed as `until` on the next cycle so
    /// each poll fetches only *new* transactions instead of re-scanning history.
    last_signature: Mutex<Option<Signature>>,
}

impl Aggregator {
    /// Creates a new `Aggregator` instance with the specified Solana RPC URL and
    /// in-memory database.
    ///
    /// # Arguments
    ///
    /// * `url` - A string slice representing the URL of the Solana RPC endpoint.
    /// * `db` - A thread-safe reference to an `InMemoryDatabase` instance.
    /// * `fetch_limit` - Max number of signatures to request per cycle. Keeping
    ///   this bounded is what stops a busy account (which can return up to 1000
    ///   signatures) from blowing the per-cycle timeout.
    ///
    /// # Returns
    ///
    /// A new instance of `Aggregator`.
    pub fn new(url: &str, db: Arc<InMemoryDatabase>, fetch_limit: usize) -> Self {
        let client = RpcClient::new(url.to_string());
        Self {
            client,
            db,
            fetch_limit: fetch_limit.max(1),
            last_signature: Mutex::new(None),
        }
    }

    /// Fetches the start time (Unix timestamp) of the current Solana epoch.
    ///
    /// # Returns
    ///
    /// A result containing the epoch start time in seconds since Unix epoch, or an
    /// `AggregatorError` if an error occurs.
    async fn get_epoch_start_time(&self) -> Result<i64, AggregatorError> {
        let epoch_info = self
            .client
            .get_epoch_info()
            .await
            .map_err(AggregatorError::FetchTransactionError)?;

        // Approximate time per Solana slot (in seconds)
        let block_production_time_per_slot = 0.4;

        // Calculate the start slot and its corresponding timestamp
        let slots_since_epoch_start = epoch_info.slot_index;
        let seconds_since_epoch_start =
            (slots_since_epoch_start as f64 * block_production_time_per_slot) as i64;
        let current_time = self
            .client
            .get_block_time(epoch_info.absolute_slot)
            .await
            .map_err(AggregatorError::FetchTransactionError)?;

        Ok(current_time - seconds_since_epoch_start)
    }

    /// Fetches recent transactions for the specified Solana address and stores
    /// them in the in-memory database.
    ///
    /// # Arguments
    ///
    /// * `address` - A string slice representing the Solana public key of the account.
    ///
    /// # Returns
    ///
    /// A result containing a vector of `TransactionData` if successful, or an `AggregatorError` if an error occurs.
    pub async fn fetch_recent_transactions(
        &self,
        address: &str,
    ) -> Result<Vec<TransactionData>, AggregatorError> {
        let timeout_duration = Duration::from_secs(10); // Set a timeout duration of 10 seconds

        info!("Starting transaction fetch for address: {}", address);

        // Fetch the start time of the current epoch
        let epoch_start_time = self.get_epoch_start_time().await?;

        // Only fetch what's newer than the last signature we ingested.
        let until = *self.last_signature.lock().await;

        let (transactions, newest) = timeout(timeout_duration, async {
            let pubkey: Pubkey = address
                .parse()
                .map_err(|_| AggregatorError::InvalidPublicKey)?;

            info!("Fetching signatures for address: {}", pubkey);

            // Fetch a bounded page of recent signatures (newest first), stopping
            // at `until` so repeat cycles only see new activity.
            let config = GetConfirmedSignaturesForAddress2Config {
                before: None,
                until,
                limit: Some(self.fetch_limit),
                commitment: None,
            };
            let signatures = self
                .client
                .get_signatures_for_address_with_config(&pubkey, config)
                .await
                .map_err(AggregatorError::FetchSignaturesError)?;

            info!(
                "Fetched {} signatures for address: {}",
                signatures.len(),
                pubkey
            );

            // The first entry is the newest; remember it so the next cycle can
            // resume from here.
            let newest = signatures
                .first()
                .and_then(|s| s.signature.parse::<Signature>().ok());

            // Fetch + decode the per-signature details concurrently (bounded), so
            // the cycle isn't a slow serial chain of RPC round-trips.
            let transactions: Vec<TransactionData> = futures::stream::iter(signatures)
                .map(|sig_info| self.decode_signature(sig_info, epoch_start_time))
                .buffer_unordered(FETCH_CONCURRENCY)
                .filter_map(|decoded| async move { decoded })
                .collect()
                .await;

            // Persist each decoded transaction (idempotent by signature).
            for tx in &transactions {
                self.db.add_transaction(address, tx.clone()).await;
            }

            Ok::<(Vec<TransactionData>, Option<Signature>), AggregatorError>((transactions, newest))
        })
        .await??;

        // Advance the cursor only after a fully successful cycle.
        if let Some(sig) = newest {
            *self.last_signature.lock().await = Some(sig);
        }

        info!(
            "Transaction fetch completed successfully for address: {} ({} new)",
            address,
            transactions.len()
        );

        Ok(transactions)
    }

    /// Fetches and decodes a single transaction. Returns `None` (with a log)
    /// rather than aborting the whole cycle on a per-signature failure — a bad
    /// signature, an RPC hiccup, or a pre-epoch/unsupported transaction just
    /// gets skipped.
    async fn decode_signature(
        &self,
        signature_info: RpcConfirmedTransactionStatusWithSignature,
        epoch_start_time: i64,
    ) -> Option<TransactionData> {
        let signature: Signature = match signature_info.signature.parse() {
            Ok(sig) => sig,
            Err(_) => {
                warn!(
                    "Skipping unparseable signature: {}",
                    signature_info.signature
                );
                return None;
            }
        };

        let tx = self
            .client
            .get_transaction(&signature, UiTransactionEncoding::JsonParsed)
            .await
            .ok()?;

        let block_time = tx.block_time?;
        // Process only transactions from the current epoch.
        if block_time < epoch_start_time {
            return None;
        }

        let meta = tx.transaction.meta.as_ref()?;
        let EncodedTransaction::Json(transaction) = &tx.transaction.transaction else {
            return None;
        };
        let UiTransaction { message, .. } = transaction;
        let (sender, receiver) = match message {
            UiMessage::Parsed(parsed_message) => {
                let sender = parsed_message
                    .account_keys
                    .first()
                    .map_or("unknown".to_string(), |acc| acc.pubkey.clone());
                let receiver = parsed_message
                    .account_keys
                    .get(1)
                    .map_or("unknown".to_string(), |acc| acc.pubkey.clone());
                (sender, receiver)
            }
            UiMessage::Raw(raw_message) => {
                let sender = raw_message
                    .account_keys
                    .first()
                    .map_or("unknown".to_string(), |key| key.clone());
                let receiver = raw_message
                    .account_keys
                    .get(1)
                    .map_or("unknown".to_string(), |key| key.clone());
                (sender, receiver)
            }
        };
        let amount = balance_delta(&meta.pre_balances, &meta.post_balances);

        Some(TransactionData {
            signature: signature_info.signature,
            sender,
            receiver,
            amount,
            timestamp: block_time as u64,
        })
    }
}

/// Lamport magnitude moved for account index 1, computed without panicking.
///
/// The previous `post_balances[1] - pre_balances[1]` had two bugs: it indexed
/// `[1]` unchecked (panics on transactions with fewer than two accounts), and
/// the `u64` subtraction underflowed for the common case where the balance
/// *decreases* (panic in debug, silent wrap in release). Using a signed
/// difference and taking the absolute value yields the transferred magnitude
/// for either direction; a missing index yields 0.
fn balance_delta(pre_balances: &[u64], post_balances: &[u64]) -> u64 {
    match (pre_balances.get(1), post_balances.get(1)) {
        (Some(&pre), Some(&post)) => (post as i64 - pre as i64).unsigned_abs(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {

    use super::balance_delta;
    use crate::db::{InMemoryDatabase, TransactionData};
    use std::sync::Arc;

    #[test]
    fn test_balance_delta_handles_decrease_and_missing_index() {
        // Incoming: post > pre.
        assert_eq!(balance_delta(&[10, 5], &[10, 12]), 7);
        // Outgoing: post < pre — previously underflowed/panicked.
        assert_eq!(balance_delta(&[10, 12], &[10, 5]), 7);
        // Fewer than two accounts — previously indexed out of bounds.
        assert_eq!(balance_delta(&[10], &[10]), 0);
        assert_eq!(balance_delta(&[], &[]), 0);
    }

    /// Test to verify that the `Aggregator` can add a transaction to the in-memory
    /// database and retrieve it correctly.
    #[tokio::test]
    async fn test_aggregator_add_and_fetch_transaction() {
        // Initialize the in-memory database
        let db = Arc::new(InMemoryDatabase::new("test_transactions.txt".to_string()));

        // Create a mock transaction
        let transaction = TransactionData {
            signature: "test_signature".to_string(),
            sender: "sender1".to_string(),
            receiver: "receiver1".to_string(),
            amount: 100,
            timestamp: 1628500000,
        };

        // Add the transaction to the database
        db.add_transaction("sender1", transaction.clone()).await;

        // Fetch the transactions for the sender
        let transactions = db.get_transactions("sender1").await;

        // Verify that the transaction is correctly stored and retrieved
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0], transaction);
    }
}
