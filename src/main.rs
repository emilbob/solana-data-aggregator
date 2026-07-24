mod aggregator;
mod api;
mod db;

use aggregator::Aggregator;
use api::create_api;
use db::InMemoryDatabase;
use dotenv::dotenv;
use env_logger::Env;
use log::{error, info};
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::Mutex;
use tokio::time::Duration;

#[tokio::main]
async fn main() {
    // Initialize the logger from environment variables, defaulting to "info" level
    env_logger::init_from_env(Env::default().default_filter_or("info"));

    // Load environment variables from a .env file, if present
    dotenv().ok();

    // Retrieve the RPC URL and public key from environment variables
    let rpc_url = env::var("SOLANA_RPC_URL").expect("SOLANA_RPC_URL must be set");
    let pub_key = env::var("SOLANA_PUBLIC_KEY").expect("SOLANA_PUBLIC_KEY must be set");

    // Initialize the in-memory database with a file path for persistence
    let db = Arc::new(InMemoryDatabase::new("transactions.txt".to_string()));

    // Load data from the file into the in-memory database
    db.load_from_file().await;

    // Initialize the aggregator with the RPC URL and the database reference
    let aggregator = Arc::new(Mutex::new(Aggregator::new(&rpc_url, db.clone())));

    // Refresh callback for /refresh endpoint
    let aggregator_clone = aggregator.clone();
    let pub_key_clone = pub_key.clone();
    let refresh_callback: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let aggregator = aggregator_clone.clone();
        let pub_key = pub_key_clone.clone();
        tokio::spawn(async move {
            let locked_aggregator = aggregator.lock().await;
            let _ = locked_aggregator.fetch_recent_transactions(&pub_key).await;
        });
    });

    info!("Starting Solana Data Aggregator...");

    // Set up a one-shot channel for shutdown signaling
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // Create the API and bind it to the configured address (SERVER_ADDR,
    // default 127.0.0.1:3030).
    let api = create_api(db.clone(), rpc_url.clone(), refresh_callback);
    let addr: SocketAddr = env::var("SERVER_ADDR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| ([127, 0, 0, 1], 3030).into());

    // Start the Warp server with graceful shutdown wired to the oneshot: when
    // `shutdown_tx` fires, warp stops accepting connections and drains in-flight
    // requests instead of being killed mid-response.
    let mut warp_server_task = tokio::spawn(async move {
        warp::serve(api)
            .bind(addr)
            .await
            .graceful(async move {
                shutdown_rx.await.ok();
            })
            .run()
            .await;
    });
    info!("API listening on http://{}", addr);

    // Task to periodically fetch recent transactions from the Solana blockchain
    let fetch_task = tokio::spawn(async move {
        loop {
            let locked_aggregator = aggregator.lock().await;
            match locked_aggregator.fetch_recent_transactions(&pub_key).await {
                Ok(transactions) => {
                    let limited_transactions =
                        &transactions[..std::cmp::min(5, transactions.len())];
                    info!("Fetched {} transactions", limited_transactions.len());
                }
                Err(err) => error!("Error fetching transactions: {:?}", err),
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });

    let mut fetch_task = Some(fetch_task);

    // Gracefully handle shutdown signals
    tokio::select! {
        _ = signal::ctrl_c() => {
            info!("Received Ctrl+C, shutting down gracefully...");

            // Abort the fetch task if it is running
            if let Some(task) = fetch_task.take() {
                task.abort();
                info!("Fetch task aborted");
            }

            // Signal the Warp server to drain and stop.
            let _ = shutdown_tx.send(());
            info!("Sent shutdown signal to Warp server");

            // Give in-flight requests up to 5s to drain, then force the issue.
            match tokio::time::timeout(Duration::from_secs(5), &mut warp_server_task).await {
                Ok(_) => info!("Warp server shut down cleanly."),
                Err(_) => {
                    warp_server_task.abort();
                    info!("Warp shutdown timed out after 5s; aborted.");
                }
            }
        },
        _ = &mut warp_server_task => {
            info!("Warp server task completed.");
            if let Some(task) = fetch_task.take() {
                task.abort();
            }
        },
    }

    info!("Shutdown process finished.");
}
