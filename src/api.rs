use crate::store::Store;
use chrono::{NaiveDate, TimeZone, Utc};
use log::{error, info};
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use std::sync::Arc;
use std::time::Instant;
use warp::filters::BoxedFilter;
use warp::Filter;

/// A monitored cluster, as the API needs it. `rpc_url` is used server-side for
/// the balance endpoint and is never serialized to clients.
#[derive(Clone)]
pub struct NetworkApi {
    pub name: String,
    pub rpc_url: String,
    pub accounts: Vec<String>,
}

/// Query parameters for `/transactions`.
#[derive(Debug, Deserialize)]
pub struct TransactionQueryParams {
    pub pub_key: String,
    pub network: Option<String>, // Cluster; defaults to the first configured network
    pub day: Option<String>,     // Optional date filter in "dd/mm/yyyy" format
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// Query parameter carrying just an optional network (for `/accounts`, balance).
#[derive(Debug, Deserialize)]
pub struct NetworkQuery {
    pub network: Option<String>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct BalanceResponse {
    pub_key: String,
    balance: u64,
}

#[derive(Serialize)]
struct NetworkInfo {
    name: String,
    accounts: Vec<String>,
}

/// Picks the network by name, or falls back to the first configured one.
fn resolve_network<'a>(
    networks: &'a [NetworkApi],
    name: &Option<String>,
) -> Option<&'a NetworkApi> {
    match name {
        Some(n) => networks.iter().find(|net| &net.name == n),
        None => networks.first(),
    }
}

pub fn create_api(
    db: Arc<Store>,
    networks: Vec<NetworkApi>,
    started: Instant,
    refresh_callback: Arc<dyn Fn() + Send + Sync>,
) -> BoxedFilter<(impl warp::Reply,)> {
    let networks = Arc::new(networks);
    let db_filter = warp::any().map(move || db.clone());
    let nets = networks.clone();
    let networks_filter = warp::any().map(move || nets.clone());
    let started_filter = warp::any().map(move || started);
    let refresh_callback_filter = warp::any().map(move || refresh_callback.clone());

    // Static dashboard (single self-contained page, embedded in the binary) at GET /
    let index = warp::path::end()
        .and(warp::get())
        .map(|| warp::reply::html(include_str!("index.html")));

    // /health endpoint
    let health = warp::path("health")
        .and(warp::get())
        .map(|| warp::reply::json(&HealthResponse { status: "ok" }));

    // /metrics endpoint (Prometheus text exposition)
    let metrics = warp::path("metrics")
        .and(warp::get())
        .and(db_filter.clone())
        .and(networks_filter.clone())
        .and(started_filter)
        .and_then(handle_metrics);

    // /networks — list of monitored clusters (name + accounts; never the rpc_url)
    let networks_list = warp::path("networks")
        .and(warp::path::end())
        .and(warp::get())
        .and(networks_filter.clone())
        .and_then(handle_networks);

    // /transactions (list)
    let transactions = warp::path("transactions")
        .and(warp::path::end())
        .and(warp::get())
        .and(warp::query::<TransactionQueryParams>())
        .and(db_filter.clone())
        .and(networks_filter.clone())
        .and_then(handle_get_transactions);

    // /transactions/{signature}
    let transaction_by_sig = warp::path!("transactions" / String)
        .and(warp::get())
        .and(db_filter.clone())
        .and_then(handle_get_transaction_by_signature);

    // /accounts (list monitored accounts, optionally for a given ?network=)
    let accounts_list = warp::path("accounts")
        .and(warp::path::end())
        .and(warp::get())
        .and(warp::query::<NetworkQuery>())
        .and(networks_filter.clone())
        .and_then(handle_accounts);

    // /accounts/{pub_key}/balance
    let account_balance = warp::path!("accounts" / String / "balance")
        .and(warp::get())
        .and(warp::query::<NetworkQuery>())
        .and(networks_filter.clone())
        .and_then(handle_get_account_balance);

    // /refresh (POST)
    let refresh = warp::path("refresh")
        .and(warp::post())
        .and(refresh_callback_filter.clone())
        .map(|refresh_callback: Arc<dyn Fn() + Send + Sync>| {
            (refresh_callback)();
            warp::reply::json(&serde_json::json!({"status": "refresh triggered"}))
        });

    index
        .or(health)
        .or(metrics)
        .or(networks_list)
        .or(accounts_list)
        .or(transactions)
        .or(transaction_by_sig)
        .or(account_balance)
        .or(refresh)
        .boxed()
}

/// Handles GET /networks — the monitored clusters (name + accounts only).
async fn handle_networks(
    networks: Arc<Vec<NetworkApi>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let out: Vec<NetworkInfo> = networks
        .iter()
        .map(|n| NetworkInfo {
            name: n.name.clone(),
            accounts: n.accounts.clone(),
        })
        .collect();
    Ok(warp::reply::json(&out))
}

/// Handles GET /accounts — accounts for the requested (or first) network.
async fn handle_accounts(
    q: NetworkQuery,
    networks: Arc<Vec<NetworkApi>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let accounts = resolve_network(&networks, &q.network)
        .map(|n| n.accounts.clone())
        .unwrap_or_default();
    Ok(warp::reply::json(&accounts))
}

/// Handles GET /metrics — Prometheus text exposition of basic service metrics.
async fn handle_metrics(
    db: Arc<Store>,
    networks: Arc<Vec<NetworkApi>>,
    started: Instant,
) -> Result<impl warp::Reply, warp::Rejection> {
    let total = db.count().await;
    let uptime = started.elapsed().as_secs();
    let account_count: usize = networks.iter().map(|n| n.accounts.len()).sum();
    let body = format!(
        "# HELP solana_aggregator_transactions_total Total transactions stored.\n\
         # TYPE solana_aggregator_transactions_total gauge\n\
         solana_aggregator_transactions_total {total}\n\
         # HELP solana_aggregator_monitored_accounts Number of monitored accounts (all networks).\n\
         # TYPE solana_aggregator_monitored_accounts gauge\n\
         solana_aggregator_monitored_accounts {account_count}\n\
         # HELP solana_aggregator_monitored_networks Number of monitored networks.\n\
         # TYPE solana_aggregator_monitored_networks gauge\n\
         solana_aggregator_monitored_networks {}\n\
         # HELP solana_aggregator_uptime_seconds Seconds since the service started.\n\
         # TYPE solana_aggregator_uptime_seconds counter\n\
         solana_aggregator_uptime_seconds {uptime}\n",
        networks.len()
    );
    Ok(warp::reply::with_header(
        body,
        "content-type",
        "text/plain; version=0.0.4",
    ))
}
/// Handles GET /transactions/{signature}
async fn handle_get_transaction_by_signature(
    signature: String,
    db: Arc<Store>,
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

/// Handles GET /accounts/{pub_key}/balance — uses the requested network's RPC.
async fn handle_get_account_balance(
    pub_key: String,
    q: NetworkQuery,
    networks: Arc<Vec<NetworkApi>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let Some(net) = resolve_network(&networks, &q.network) else {
        return Ok(warp::reply::json(
            &serde_json::json!({"error": "Unknown network"}),
        ));
    };
    let client = RpcClient::new(net.rpc_url.clone());
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
/// * `db` - A thread-safe reference to the storage backend.
///
/// # Returns
///
/// A JSON response containing the filtered transactions or an error message.
async fn handle_get_transactions(
    params: TransactionQueryParams,
    db: Arc<Store>,
    networks: Arc<Vec<NetworkApi>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    // Resolve the network (query param, or the first configured one).
    let network = resolve_network(&networks, &params.network)
        .map(|n| n.name.clone())
        .unwrap_or_default();
    info!("Request for {} on network '{}'", params.pub_key, network);

    // Retrieve all transactions for (network, public key)
    let transactions = db.get_transactions(&network, &params.pub_key).await;

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
    use crate::db::{TransactionData, Transfer};
    use crate::store::Store;
    use warp::test::request;

    /// Test to verify that the API correctly handles fetching transactions with mock data.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_api_get_transactions_with_mock_data() {
        let db = Arc::new(Store::memory("mock_test_transactions.txt"));

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
            token_changes: vec![],
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
            token_changes: vec![],
        };

        // Add transactions to the in-memory database (under network "mainnet")
        db.add_transaction("mainnet", "mock_sender_1", transaction1.clone())
            .await;
        db.add_transaction("mainnet", "mock_sender_2", transaction2.clone())
            .await;

        // Create the API with one network ("mainnet") and a no-op refresh callback
        let api = create_api(
            db.clone(),
            vec![NetworkApi {
                name: "mainnet".to_string(),
                rpc_url: "mock_rpc_url".to_string(),
                accounts: vec!["mock_sender_1".to_string(), "mock_sender_2".to_string()],
            }],
            Instant::now(),
            Arc::new(|| {}),
        );

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
