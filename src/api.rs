use crate::db::InMemoryDatabase;
use chrono::{NaiveDate, TimeZone, Utc};
use log::{error, info};
use serde::Deserialize;
use std::sync::Arc;
use warp::Filter;

/// Struct to define the query parameters for the API requests.
#[derive(Debug, Deserialize)]
pub struct TransactionQueryParams {
    pub pub_key: String, // The public key of the account to fetch transactions for
    pub day: Option<String>, // Optional date filter in "dd/mm/yyyy" format
    pub limit: Option<usize>, // Optional limit on the number of transactions to return
    pub offset: Option<usize>, // Optional pagination offset
}

use serde::Serialize;
/// Creates the API with enhanced querying capabilities.
///
/// # Arguments
///
/// * `db` - A thread-safe reference to an `InMemoryDatabase`.
///
/// # Returns
///
/// A warp filter that handles incoming HTTP requests to fetch transactions.
use solana_client::nonblocking::rpc_client::RpcClient;
use warp::filters::BoxedFilter;

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct BalanceResponse {
    pub_key: String,
    balance: u64,
}

pub fn create_api(
    db: Arc<InMemoryDatabase>,
    rpc_url: String,
    refresh_callback: Arc<dyn Fn() + Send + Sync>,
) -> BoxedFilter<(impl warp::Reply,)> {
    let db_filter = warp::any().map(move || db.clone());
    let rpc_url_filter = warp::any().map(move || rpc_url.clone());
    let refresh_callback_filter = warp::any().map(move || refresh_callback.clone());

    // /health endpoint
    let health = warp::path("health")
        .and(warp::get())
        .map(|| warp::reply::json(&HealthResponse { status: "ok" }));

    // /transactions (list)
    let transactions = warp::path("transactions")
        .and(warp::get())
        .and(warp::query::<TransactionQueryParams>())
        .and(db_filter.clone())
        .and_then(handle_get_transactions);

    // /transactions/{signature}
    let transaction_by_sig = warp::path!("transactions" / String)
        .and(warp::get())
        .and(db_filter.clone())
        .and_then(handle_get_transaction_by_signature);

    // /accounts/{pub_key}/balance
    let account_balance = warp::path!("accounts" / String / "balance")
        .and(warp::get())
        .and(rpc_url_filter.clone())
        .and_then(handle_get_account_balance);

    // /refresh (POST)
    let refresh = warp::path("refresh")
        .and(warp::post())
        .and(refresh_callback_filter.clone())
        .map(|refresh_callback: Arc<dyn Fn() + Send + Sync>| {
            (refresh_callback)();
            warp::reply::json(&serde_json::json!({"status": "refresh triggered"}))
        });

    health
        .or(transactions)
        .or(transaction_by_sig)
        .or(account_balance)
        .or(refresh)
        .boxed()
}
/// Handles GET /transactions/{signature}
async fn handle_get_transaction_by_signature(
    signature: String,
    db: Arc<InMemoryDatabase>,
) -> Result<impl warp::Reply, warp::Rejection> {
    if let Some(tx) = db.get_transaction_by_signature(&signature).await {
        Ok(warp::reply::json(&tx))
    } else {
        let error_message = warp::reply::json(&serde_json::json!({
            "error": "Transaction not found"
        }));
        Ok(error_message)
    }
}

/// Handles GET /accounts/{pub_key}/balance
async fn handle_get_account_balance(
    pub_key: String,
    rpc_url: String,
) -> Result<impl warp::Reply, warp::Rejection> {
    let client = RpcClient::new(rpc_url);
    match pub_key.parse() {
        Ok(pubkey) => match client.get_balance(&pubkey).await {
            Ok(balance) => Ok(warp::reply::json(&BalanceResponse { pub_key, balance })),
            Err(e) => {
                let error_message = warp::reply::json(&serde_json::json!({
                    "error": format!("Failed to fetch balance: {}", e)
                }));
                Ok(error_message)
            }
        },
        Err(_) => {
            let error_message = warp::reply::json(&serde_json::json!({
                "error": "Invalid public key format"
            }));
            Ok(error_message)
        }
    }
}

/// Handles incoming API requests to fetch transactions.
///
/// # Arguments
///
/// * `params` - The query parameters provided by the client.
/// * `db` - A thread-safe reference to an `InMemoryDatabase`.
///
/// # Returns
///
/// A JSON response containing the filtered transactions or an error message.
async fn handle_get_transactions(
    params: TransactionQueryParams,
    db: Arc<InMemoryDatabase>,
) -> Result<impl warp::Reply, warp::Rejection> {
    info!("Received request for public key: {}", params.pub_key);

    // Retrieve all transactions for the given public key
    let transactions = db.get_transactions(&params.pub_key).await;

    // Filter transactions by date if the `day` parameter is provided
    let filtered_transactions = if let Some(ref day) = params.day {
        if let Ok(date_filter) = parse_date(day) {
            transactions
                .into_iter()
                .filter(|tx| is_same_day(tx.timestamp, date_filter))
                .collect()
        } else {
            error!("Invalid date format: {}", day);
            let error_message = warp::reply::json(&serde_json::json!({
                "error": "Invalid date format",
                "details": "Please use the format dd/mm/yyyy."
            }));
            return Ok(error_message);
        }
    } else {
        transactions
    };

    // Apply pagination based on `limit` and `offset` parameters
    let total = filtered_transactions.len();
    let limit = params.limit.unwrap_or(5); // Default limit is 5
    let offset = params.offset.unwrap_or(0); // Default offset is 0
    let limited_transactions = filtered_transactions
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();

    info!(
        "Returning {} transactions (total: {}) for public key: {}",
        limited_transactions.len(),
        total,
        params.pub_key
    );

    Ok(warp::reply::json(&limited_transactions))
}

/// Parses a date string in "dd/mm/yyyy" format into a `NaiveDate`.
///
/// # Arguments
///
/// * `date_str` - The date string to parse.
///
/// # Returns
///
/// A result containing a `NaiveDate` if the parsing was successful, or a `ParseError` otherwise.
fn parse_date(date_str: &str) -> Result<NaiveDate, chrono::format::ParseError> {
    NaiveDate::parse_from_str(date_str, "%d/%m/%Y")
}

/// Checks if a transaction's timestamp matches a specific date.
///
/// # Arguments
///
/// * `timestamp` - The Unix timestamp of the transaction.
/// * `date` - The date to compare against.
///
/// # Returns
///
/// `true` if the transaction occurred on the specified date, `false` otherwise.
fn is_same_day(timestamp: u64, date: NaiveDate) -> bool {
    let datetime = Utc.timestamp_opt(timestamp as i64, 0).single();
    if let Some(transaction_date) = datetime {
        transaction_date.date_naive() == date
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{InMemoryDatabase, TransactionData, Transfer};
    use warp::test::request;

    /// Test to verify that the API correctly handles fetching transactions with mock data.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_api_get_transactions_with_mock_data() {
        let db = Arc::new(InMemoryDatabase::new(
            "mock_test_transactions.txt".to_string(),
        ));

        // Mock some transaction data
        let transaction1 = TransactionData {
            signature: "mock_sig_1".to_string(),
            slot: 10,
            timestamp: 1628500000,
            fee: 5000,
            fee_payer: "mock_sender_1".to_string(),
            success: true,
            tx_type: "transfer".to_string(),
            programs: vec!["system".to_string()],
            transfer: Some(Transfer {
                source: "mock_sender_1".to_string(),
                destination: "mock_receiver_1".to_string(),
                lamports: 1000,
            }),
        };

        let transaction2 = TransactionData {
            signature: "mock_sig_2".to_string(),
            slot: 11,
            timestamp: 1628501000,
            fee: 5000,
            fee_payer: "mock_sender_2".to_string(),
            success: true,
            tx_type: "vote".to_string(),
            programs: vec!["vote".to_string()],
            transfer: None,
        };

        // Add transactions to the in-memory database
        db.add_transaction("mock_sender_1", transaction1.clone())
            .await;
        db.add_transaction("mock_sender_2", transaction2.clone())
            .await;

        // Create the API with the mocked database, dummy rpc_url, and no-op refresh callback
        let api = create_api(db.clone(), "mock_rpc_url".to_string(), Arc::new(|| {}));

        // Query the API for the first transaction
        let response1 = request()
            .path("/transactions?pub_key=mock_sender_1")
            .reply(&api)
            .await;

        assert_eq!(response1.status(), 200);
        let body1: Vec<TransactionData> = serde_json::from_slice(response1.body()).unwrap();
        assert_eq!(body1.len(), 1);
        assert_eq!(body1[0], transaction1);

        // Query the API for the second transaction
        let response2 = request()
            .path("/transactions?pub_key=mock_sender_2")
            .reply(&api)
            .await;

        assert_eq!(response2.status(), 200);
        let body2: Vec<TransactionData> = serde_json::from_slice(response2.body()).unwrap();
        assert_eq!(body2.len(), 1);
        assert_eq!(body2[0], transaction2);
    }
}
