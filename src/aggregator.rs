use crate::db::{TransactionData, Transfer};
use crate::store::Store;
use futures::stream::StreamExt;
use log::{info, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_client::rpc_response::RpcConfirmedTransactionStatusWithSignature;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_transaction_status::option_serializer::OptionSerializer;
use solana_transaction_status::{
    EncodedTransaction, UiInstruction, UiMessage, UiParsedInstruction, UiTransaction,
    UiTransactionEncoding, UiTransactionStatusMeta,
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
/// them via the configured storage backend.
pub struct Aggregator {
    client: RpcClient,  // Solana RPC client used to interact with the blockchain
    db: Arc<Store>,     // Storage backend (in-memory or Postgres)
    fetch_limit: usize, // Max signatures to pull per cycle (bounds the per-cycle work)
    timeout: Duration,  // Per-cycle budget for the signature + detail fetches
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
    /// * `db` - A thread-safe reference to the storage backend.
    /// * `fetch_limit` - Max number of signatures to request per cycle. Keeping
    ///   this bounded is what stops a busy account (which can return up to 1000
    ///   signatures) from blowing the per-cycle timeout.
    /// * `timeout_secs` - Per-cycle budget (seconds) for the signature + detail
    ///   fetches. Raise it to grind through a slow or rate-limited RPC.
    ///
    /// # Returns
    ///
    /// A new instance of `Aggregator`.
    pub fn new(url: &str, db: Arc<Store>, fetch_limit: usize, timeout_secs: u64) -> Self {
        let client = RpcClient::new(url.to_string());
        Self {
            client,
            db,
            fetch_limit: fetch_limit.max(1),
            timeout: Duration::from_secs(timeout_secs.max(1)),
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
        let timeout_duration = self.timeout; // Per-cycle budget (configurable)

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

            // Persist each decoded transaction (idempotent by signature) and log
            // a readable summary so the terminal narrates what was ingested.
            for tx in &transactions {
                self.db.add_transaction(address, tx.clone()).await;
                info!("ingested {}", tx.summary());
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
        let success = meta.err.is_none();
        let fee = meta.fee;
        let slot = tx.slot;

        let EncodedTransaction::Json(transaction) = &tx.transaction.transaction else {
            return None;
        };
        let UiTransaction { message, .. } = transaction;

        // Fee payer is the first account; programs invoked and any native SOL
        // transfer come from the parsed instructions (we request JsonParsed).
        let (fee_payer, programs, transfer) = match message {
            UiMessage::Parsed(m) => {
                let fee_payer = m
                    .account_keys
                    .first()
                    .map_or_else(|| "unknown".to_string(), |a| a.pubkey.clone());
                let mut programs: Vec<String> = Vec::new();
                let mut transfer = None;
                for ix in &m.instructions {
                    match ix {
                        UiInstruction::Parsed(UiParsedInstruction::Parsed(p)) => {
                            if !programs.contains(&p.program) {
                                programs.push(p.program.clone());
                            }
                            if transfer.is_none() {
                                transfer = parse_sol_transfer(&p.program, &p.parsed);
                            }
                        }
                        UiInstruction::Parsed(UiParsedInstruction::PartiallyDecoded(pd)) => {
                            if !programs.contains(&pd.program_id) {
                                programs.push(pd.program_id.clone());
                            }
                        }
                        UiInstruction::Compiled(_) => {}
                    }
                }
                (fee_payer, programs, transfer)
            }
            UiMessage::Raw(m) => {
                let fee_payer = m
                    .account_keys
                    .first()
                    .map_or_else(|| "unknown".to_string(), |k| k.clone());
                (fee_payer, Vec::new(), None)
            }
        };

        let tx_type = classify(&transfer, &programs, token_balances_present(meta));

        Some(TransactionData {
            signature: signature_info.signature,
            slot,
            timestamp: block_time as u64,
            fee,
            fee_payer,
            success,
            tx_type,
            programs,
            transfer,
        })
    }
}

/// Extracts a native SOL transfer from a parsed System Program instruction.
/// Returns `None` for anything that isn't a `system` `transfer` — so the stored
/// `lamports` is the actual transferred amount, not a balance-delta guess.
fn parse_sol_transfer(program: &str, parsed: &serde_json::Value) -> Option<Transfer> {
    if program != "system" || parsed.get("type")?.as_str()? != "transfer" {
        return None;
    }
    let info = parsed.get("info")?;
    Some(Transfer {
        source: info.get("source")?.as_str()?.to_string(),
        destination: info.get("destination")?.as_str()?.to_string(),
        lamports: info.get("lamports")?.as_u64()?,
    })
}

/// True if the transaction touched any SPL token balances.
fn token_balances_present(meta: &UiTransactionStatusMeta) -> bool {
    fn non_empty<T>(o: &OptionSerializer<Vec<T>>) -> bool {
        matches!(o, OptionSerializer::Some(v) if !v.is_empty())
    }
    non_empty(&meta.pre_token_balances) || non_empty(&meta.post_token_balances)
}

/// Classifies a transaction: a real SOL transfer wins; then vote; then token;
/// otherwise the first program it invoked, or "unknown".
fn classify(transfer: &Option<Transfer>, programs: &[String], has_tokens: bool) -> String {
    if transfer.is_some() {
        "transfer".to_string()
    } else if programs.iter().any(|p| p == "vote") {
        "vote".to_string()
    } else if has_tokens || programs.iter().any(|p| p == "spl-token") {
        "token".to_string()
    } else {
        programs
            .first()
            .cloned()
            .unwrap_or_else(|| "unknown".to_string())
    }
}

#[cfg(test)]
mod tests {

    use super::{classify, parse_sol_transfer};
    use crate::db::{InMemoryDatabase, TransactionData, Transfer};
    use std::sync::Arc;

    #[test]
    fn test_parse_sol_transfer_extracts_real_amount() {
        let parsed = serde_json::json!({
            "type": "transfer",
            "info": {"source": "AAA", "destination": "BBB", "lamports": 1234u64}
        });
        assert_eq!(
            parse_sol_transfer("system", &parsed),
            Some(Transfer {
                source: "AAA".to_string(),
                destination: "BBB".to_string(),
                lamports: 1234,
            })
        );
        // Not the System Program, or not a transfer → None.
        assert!(parse_sol_transfer("vote", &parsed).is_none());
        let create = serde_json::json!({"type": "createAccount", "info": {}});
        assert!(parse_sol_transfer("system", &create).is_none());
    }

    #[test]
    fn test_classify_priority() {
        let transfer = Some(Transfer {
            source: "a".to_string(),
            destination: "b".to_string(),
            lamports: 1,
        });
        assert_eq!(
            classify(&transfer, &["system".to_string()], false),
            "transfer"
        );
        assert_eq!(classify(&None, &["vote".to_string()], false), "vote");
        assert_eq!(classify(&None, &["spl-token".to_string()], false), "token");
        assert_eq!(classify(&None, &[], true), "token");
        assert_eq!(classify(&None, &["custom".to_string()], false), "custom");
        assert_eq!(classify(&None, &[], false), "unknown");
    }

    /// Test to verify that the `Aggregator` can add a transaction to the in-memory
    /// database and retrieve it correctly.
    #[tokio::test]
    async fn test_aggregator_add_and_fetch_transaction() {
        let db = Arc::new(InMemoryDatabase::new("test_transactions.txt".to_string()));

        let transaction = TransactionData {
            signature: "test_signature".to_string(),
            slot: 1,
            timestamp: 1628500000,
            fee: 5000,
            fee_payer: "sender1".to_string(),
            success: true,
            tx_type: "transfer".to_string(),
            programs: vec!["system".to_string()],
            transfer: Some(Transfer {
                source: "sender1".to_string(),
                destination: "receiver1".to_string(),
                lamports: 100,
            }),
        };

        db.add_transaction("sender1", transaction.clone()).await;

        let transactions = db.get_transactions("sender1").await;
        assert_eq!(transactions.len(), 1);
        assert_eq!(transactions[0], transaction);
    }
}
