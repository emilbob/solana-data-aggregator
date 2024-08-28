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

    info!("Starting Solana Data Aggregator...");

    // Set up a one-shot channel for shutdown signaling
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // Create the API and bind it to the specified address
    let api = create_api(db.clone());
    let addr: SocketAddr = ([127, 0, 0, 1], 3030).into();

    // Start the Warp server with graceful shutdown capability
    let (_, warp_server_future) = warp::serve(api).bind_with_graceful_shutdown(addr, async {
        shutdown_rx.await.ok();
    });

    // Spawn the Warp server task
    let warp_server_task = tokio::spawn(warp_server_future);

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

            // Send a shutdown signal to the Warp server
            let _ = shutdown_tx.send(());
            info!("Sent shutdown signal to Warp server");

            // Wait for 5 seconds to complete shutdown; otherwise, force exit
            tokio::time::sleep(Duration::from_secs(5)).await;
            info!("Forcing shutdown after timeout...");
            std::process::exit(0); // Force shutdown
        },
        _ = warp_server_task => {
            info!("Warp server task completed.");
        },
    }

    info!("Shutdown process finished.");
}
