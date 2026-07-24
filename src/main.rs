mod aggregator;
mod api;
mod db;
mod store;

use aggregator::Aggregator;
use api::{create_api, NetworkApi};
use dotenv::dotenv;
use env_logger::Env;
use log::{error, info};
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use store::Store;
use tokio::signal;
use tokio::time::Duration;

/// Reads a required env var, exiting with a clear message instead of panicking.
fn require_env(key: &str) -> String {
    match env::var(key) {
        Ok(val) => val,
        Err(_) => {
            eprintln!("error: {key} must be set (see README / .env)");
            std::process::exit(1);
        }
    }
}

/// Collects the monitored accounts from `SOLANA_PUBLIC_KEYS` (comma-separated)
/// and/or the single `SOLANA_PUBLIC_KEY`, de-duplicated and order-preserving.
fn collect_accounts(single: Option<String>, multi: Option<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        let s = s.trim();
        if !s.is_empty() && !out.iter().any(|e| e == s) {
            out.push(s.to_string());
        }
    };
    if let Some(m) = multi {
        for k in m.split(',') {
            push(k);
        }
    }
    if let Some(s) = single {
        push(&s);
    }
    out
}

/// Resolves the address to bind. Cloud platforms (Render, Cloud Run, Heroku…)
/// inject a `PORT` and expect the app to listen on `0.0.0.0:PORT`, which wins.
/// Otherwise honor `SERVER_ADDR`, else default to `127.0.0.1:3030`.
fn bind_addr(port: Option<String>, server_addr: Option<String>) -> SocketAddr {
    if let Some(port) = port.and_then(|p| p.trim().parse::<u16>().ok()) {
        return ([0, 0, 0, 0], port).into();
    }
    server_addr
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| ([127, 0, 0, 1], 3030).into())
}

/// Derives a WebSocket URL from an RPC URL (`https`→`wss`, `http`→`ws`).
fn derive_ws_url(rpc_url: &str) -> String {
    if let Some(rest) = rpc_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = rpc_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        rpc_url.to_string()
    }
}

/// Infers a cluster name from an RPC URL (for legacy single-network config).
fn network_name_from_url(url: &str) -> String {
    let u = url.to_lowercase();
    if u.contains("devnet") {
        "devnet"
    } else if u.contains("testnet") {
        "testnet"
    } else if u.contains("mainnet") {
        "mainnet"
    } else {
        "custom"
    }
    .to_string()
}

fn truthy(key: &str) -> bool {
    matches!(env::var(key).ok().as_deref(), Some("true") | Some("1"))
}

/// One monitored cluster: its name, RPC/WS endpoints, accounts, and whether to
/// use real-time WebSocket ingestion.
#[derive(Clone)]
struct NetworkConfig {
    name: String,
    rpc_url: String,
    ws_url: Option<String>,
    websocket: bool,
    accounts: Vec<String>,
}

/// Loads the networks to monitor. Multi-network mode is driven by `NETWORKS`
/// (comma-separated names); for each `<NAME>` it reads `<NAME>_RPC_URL`,
/// `<NAME>_PUBLIC_KEYS`/`<NAME>_PUBLIC_KEY`, and optional `<NAME>_WS_URL` /
/// `<NAME>_WEBSOCKET`. Without `NETWORKS`, it falls back to the single-network
/// `SOLANA_*` vars (name inferred from the RPC URL) — fully backward compatible.
fn load_networks() -> Vec<NetworkConfig> {
    if let Ok(list) = env::var("NETWORKS") {
        return list
            .split(',')
            .filter_map(|name| {
                let name = name.trim();
                if name.is_empty() {
                    return None;
                }
                let p = name.to_uppercase().replace('-', "_");
                let rpc_url = env::var(format!("{p}_RPC_URL"))
                    .ok()
                    .filter(|s| !s.is_empty())?;
                let accounts = collect_accounts(
                    env::var(format!("{p}_PUBLIC_KEY")).ok(),
                    env::var(format!("{p}_PUBLIC_KEYS")).ok(),
                );
                if accounts.is_empty() {
                    eprintln!("warning: network '{name}' has no accounts set — skipping");
                    return None;
                }
                let ws_url = env::var(format!("{p}_WS_URL"))
                    .ok()
                    .filter(|s| !s.is_empty());
                let websocket = ws_url.is_some() || truthy(&format!("{p}_WEBSOCKET"));
                Some(NetworkConfig {
                    name: name.to_string(),
                    rpc_url,
                    ws_url,
                    websocket,
                    accounts,
                })
            })
            .collect();
    }

    // Legacy single-network mode.
    let rpc_url = require_env("SOLANA_RPC_URL");
    let accounts = collect_accounts(
        env::var("SOLANA_PUBLIC_KEY").ok(),
        env::var("SOLANA_PUBLIC_KEYS").ok(),
    );
    if accounts.is_empty() {
        eprintln!("error: set SOLANA_PUBLIC_KEY or SOLANA_PUBLIC_KEYS (see README / .env)");
        std::process::exit(1);
    }
    let ws_url = env::var("SOLANA_WS_URL").ok().filter(|s| !s.is_empty());
    let websocket = ws_url.is_some() || truthy("WEBSOCKET");
    vec![NetworkConfig {
        name: network_name_from_url(&rpc_url),
        rpc_url,
        ws_url,
        websocket,
        accounts,
    }]
}

#[tokio::main]
async fn main() {
    // Initialize the logger from environment variables, defaulting to "info" level
    env_logger::init_from_env(Env::default().default_filter_or("info"));

    // Load environment variables from a .env file, if present
    dotenv().ok();

    // Process start, for the uptime metric.
    let started = std::time::Instant::now();

    // Networks to monitor (one or many). See `load_networks`.
    let networks = load_networks();
    if networks.is_empty() {
        eprintln!("error: no networks configured (set NETWORKS + <NAME>_RPC_URL/_PUBLIC_KEYS, or SOLANA_RPC_URL + SOLANA_PUBLIC_KEYS)");
        std::process::exit(1);
    }
    info!(
        "Monitoring {} network(s): {}",
        networks.len(),
        networks
            .iter()
            .map(|n| format!("{} ({} account(s))", n.name, n.accounts.len()))
            .collect::<Vec<_>>()
            .join(", ")
    );

    // Max signatures to pull per fetch cycle (optional; default 20). Keeping this
    // bounded is what stops a busy account from blowing the per-cycle timeout.
    let fetch_limit: usize = env::var("FETCH_LIMIT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    // Per-cycle fetch budget in seconds (optional; default 10). Raise it to grind
    // through a slow or rate-limited RPC.
    let poll_timeout_secs: u64 = env::var("POLL_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    // Select the storage backend: Postgres when DATABASE_URL is set (durable,
    // queryable, multi-run history), otherwise the zero-setup in-memory + file
    // store. The rest of the app is backend-agnostic.
    let store = match env::var("DATABASE_URL") {
        Ok(url) => match Store::connect_postgres(&url).await {
            Ok(s) => {
                info!("Using Postgres store");
                s
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        Err(_) => {
            info!("Using in-memory store (set DATABASE_URL to use Postgres)");
            Store::memory("transactions.txt")
        }
    };
    let db = Arc::new(store);

    // Load any persisted data (no-op for Postgres — already durable).
    db.load().await;

    // One Aggregator per network, each with its own RPC client + per-account
    // cursors. Interior mutability means no outer Mutex — poll, /refresh, and WS
    // run concurrently.
    let aggregators: Vec<(NetworkConfig, Arc<Aggregator>)> = networks
        .iter()
        .map(|net| {
            let agg = Arc::new(Aggregator::new(
                &net.name,
                &net.rpc_url,
                db.clone(),
                fetch_limit,
                poll_timeout_secs,
            ));
            (net.clone(), agg)
        })
        .collect();

    // Real-time WebSocket ingestion per network (where enabled), alongside polling.
    for (net, agg) in &aggregators {
        if net.websocket {
            let ws_url = net
                .ws_url
                .clone()
                .unwrap_or_else(|| derive_ws_url(&net.rpc_url));
            info!(
                "Real-time WebSocket ingestion enabled for {}: {ws_url}",
                net.name
            );
            for account in &net.accounts {
                let agg = agg.clone();
                let url = ws_url.clone();
                let acct = account.clone();
                tokio::spawn(async move { agg.subscribe_account(url, acct).await });
            }
        }
    }

    // Refresh callback for /refresh — refreshes every account on every network.
    let refresh_set: Vec<(Arc<Aggregator>, Vec<String>)> = aggregators
        .iter()
        .map(|(net, agg)| (agg.clone(), net.accounts.clone()))
        .collect();
    let refresh_callback: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let set = refresh_set.clone();
        tokio::spawn(async move {
            for (agg, accounts) in &set {
                for acct in accounts {
                    let _ = agg.fetch_recent_transactions(acct).await;
                }
            }
        });
    });

    info!("Starting Solana Data Aggregator...");

    // Set up a one-shot channel for shutdown signaling
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // Network info for the API: name + rpc_url + accounts. `rpc_url` is used
    // server-side (balance endpoint) and is never returned to clients.
    let api_networks: Vec<NetworkApi> = networks
        .iter()
        .map(|n| NetworkApi {
            name: n.name.clone(),
            rpc_url: n.rpc_url.clone(),
            accounts: n.accounts.clone(),
        })
        .collect();

    // Create the API and bind it to the configured address.
    let api = create_api(db.clone(), api_networks, started, refresh_callback);
    let addr = bind_addr(env::var("PORT").ok(), env::var("SERVER_ADDR").ok());

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

    // Poll every account on every network as a backfill safety net.
    let poll_set: Vec<(Arc<Aggregator>, Vec<String>)> = aggregators
        .iter()
        .map(|(net, agg)| (agg.clone(), net.accounts.clone()))
        .collect();
    let fetch_task = tokio::spawn(async move {
        loop {
            for (agg, accounts) in &poll_set {
                for key in accounts {
                    match agg.fetch_recent_transactions(key).await {
                        Ok(transactions) => {
                            info!(
                                "Fetched {} new transactions for {}",
                                transactions.len(),
                                key
                            )
                        }
                        Err(err) => error!("Error fetching transactions for {}: {:?}", key, err),
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::{bind_addr, collect_accounts, derive_ws_url, network_name_from_url};

    #[test]
    fn network_name_inferred_from_rpc_url() {
        assert_eq!(
            network_name_from_url("https://api.testnet.solana.com"),
            "testnet"
        );
        assert_eq!(
            network_name_from_url("https://api.devnet.solana.com"),
            "devnet"
        );
        assert_eq!(
            network_name_from_url("https://mainnet.helius-rpc.com/?api-key=x"),
            "mainnet"
        );
        assert_eq!(network_name_from_url("http://localhost:8899"), "custom");
    }

    #[test]
    fn bind_addr_prefers_port_then_server_addr() {
        // PORT (platform-injected) wins and binds all interfaces.
        assert_eq!(
            bind_addr(Some("10000".into()), Some("127.0.0.1:3030".into())).to_string(),
            "0.0.0.0:10000"
        );
        // No PORT → honor SERVER_ADDR.
        assert_eq!(
            bind_addr(None, Some("127.0.0.1:9999".into())).to_string(),
            "127.0.0.1:9999"
        );
        // Neither → default.
        assert_eq!(bind_addr(None, None).to_string(), "127.0.0.1:3030");
        // Junk PORT falls through.
        assert_eq!(
            bind_addr(Some("nope".into()), None).to_string(),
            "127.0.0.1:3030"
        );
    }

    #[test]
    fn derive_ws_url_maps_schemes() {
        assert_eq!(
            derive_ws_url("https://api.testnet.solana.com"),
            "wss://api.testnet.solana.com"
        );
        assert_eq!(
            derive_ws_url("http://localhost:8899"),
            "ws://localhost:8899"
        );
        // Unknown scheme is passed through unchanged.
        assert_eq!(derive_ws_url("wss://x.example"), "wss://x.example");
    }

    #[test]
    fn collect_accounts_merges_dedups_and_trims() {
        // Multi (comma) first, then single appended; trimmed, blanks dropped,
        // duplicates removed, order preserved.
        assert_eq!(
            collect_accounts(Some(" A ".to_string()), Some("B, C ,B,".to_string())),
            vec!["B", "C", "A"]
        );
        assert_eq!(collect_accounts(Some("X".to_string()), None), vec!["X"]);
        assert_eq!(
            collect_accounts(None, Some("X,Y".to_string())),
            vec!["X", "Y"]
        );
        assert!(collect_accounts(None, None).is_empty());
        // De-dup across single + multi.
        assert_eq!(
            collect_accounts(Some("A".to_string()), Some("A".to_string())),
            vec!["A"]
        );
    }
}
