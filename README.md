# Solana Data Aggregator

The Solana Data Aggregator is a Rust-based tool designed to collect, process, and store transaction data from the Solana blockchain. It supports real-time data retrieval, in-memory storage with persistence capabilities, and provides a RESTful API for querying transaction data.

## Table of Contents

- Overview
- Features
- Installation
  - Prerequisites
  - Clone the Repository
  - Set Up Environment Variables
  - Build the Project
  - Run the Application
- Usage
  - API Endpoints
  - GET /transactions
- Project Structure
- Persistence
- Testing
- Design Decisions
- Future Enhancements
- Contributing
- License
- Acknowledgments

## Overview

The Solana Data Aggregator is a tool that helps developers, researchers, and blockchain enthusiasts monitor and analyze transaction activity on the Solana network. By aggregating transaction data in real-time and providing an easy-to-use API, it simplifies the process of blockchain data analysis.

## Features

- Real-Time Data Retrieval: Continuously fetches the most recent transactions from the Solana blockchain.
  In-Memory Database with Persistence: Stores transactions in memory for fast access, with periodic saving to a text file for persistence across restarts.

- RESTful API: Exposes a simple API for querying transaction data by public key, date, and with pagination.
  Graceful Shutdown: Handles shutdown signals cleanly, ensuring no data is lost.
  Modular and Extensible Design: The codebase is modular, making it easy to extend and customize for different use cases.

## Installation

### Prerequisites

Before you begin, make sure you have the following installed:

Install Rust:

```
rustup
```

Solana RPC URL: Obtain an RPC URL from a provider like Helius or use the official Solana testnet.

### Clone the Repository

First, clone the repository to your local machine:

```
git clone https://github.com/emilbob/solana-data-aggregator
cd solana-data-aggregator
```

### Set Up Environment Variables

Create a .env file in the root of the project directory:

```
SOLANA_RPC_URL=https://api.testnet.solana.com
SOLANA_PUBLIC_KEY=YourPublicKeyHere
# Optional — monitor several accounts (comma-separated); merged with SOLANA_PUBLIC_KEY
# SOLANA_PUBLIC_KEYS=Key1,Key2,Key3
# Optional — defaults to 127.0.0.1:3030
SERVER_ADDR=127.0.0.1:3030
# Optional — max signatures pulled per fetch cycle (default 20)
FETCH_LIMIT=20
# Optional — per-cycle fetch budget in seconds (default 10)
POLL_TIMEOUT_SECS=10
# Optional — use Postgres instead of the in-memory + file store
# DATABASE_URL=postgres://solana:solana@localhost:5432/solana_aggregator
# Optional — real-time ingestion via WebSocket (in addition to polling)
# WEBSOCKET=true
# Optional — override the WS endpoint (otherwise derived from SOLANA_RPC_URL)
# SOLANA_WS_URL=wss://api.testnet.solana.com
```

Replace YourPublicKeyHere with the public key you want to monitor.

- `SOLANA_PUBLIC_KEY` / `SOLANA_PUBLIC_KEYS` — the account(s) to monitor. Use
  `SOLANA_PUBLIC_KEYS` (comma-separated) to watch several at once; the two are
  merged and de-duplicated, and at least one is required. Each account is polled
  independently (its own cursor) and stored/queried under its own key.
- `SERVER_ADDR` (optional) — the address the HTTP API binds to.
- `FETCH_LIMIT` (optional, default `20`) — how many recent signatures to pull
  each cycle. Each cycle fetches only transactions newer than the last one it
  ingested and decodes them concurrently, so a small limit keeps a busy account
  from exceeding the per-cycle timeout. Very active accounts on a slow/public
  RPC still benefit from a dedicated RPC provider (e.g. Helius).
- `POLL_TIMEOUT_SECS` (optional, default `10`) — per-cycle budget for the
  signature + detail fetches. Raise it if you must use a slow or rate-limited
  RPC and would rather wait than see `Elapsed`.
- `DATABASE_URL` (optional) — when set, transactions are stored in **Postgres**
  (durable, indexed, queryable) instead of the in-memory + file store. See
  [Storage backends](#persistence) below.
- `WEBSOCKET` (optional) — set to `true` to enable **real-time ingestion**: the
  service subscribes to each account's transaction logs over WebSocket and
  ingests transactions as they land, with auto-reconnect. Polling stays on as a
  backfill safety net (ingestion is idempotent, so overlap is harmless).
- `SOLANA_WS_URL` (optional) — the WebSocket endpoint. Defaults to `SOLANA_RPC_URL`
  with `http(s)` rewritten to `ws(s)`. Setting it also enables WebSocket ingestion.

### Build the Project

Use Cargo to build the project:

```
cargo build
```

### Run the Application

Run the application using the following command:

```
cargo run
```

The application will start a server at `http://127.0.0.1:3030` and begin fetching transactions from the Solana blockchain.

Open **`http://127.0.0.1:3030/`** in a browser for the built-in **web dashboard** — a
single self-contained page (embedded in the binary, served by the same warp
server) to query an account's transactions, view its balance, trigger a chain
refresh, and browse the enriched records with type badges and explorer links. No
Node, no build step, no separate server.

## Usage

Once the application is running, you can interact with it using the provided API. The server will listen on `http://127.0.0.1:3030` by default.

You can query the transactions stored in the database using the API. Refer to the API Endpoints section below for detailed information on how to make these queries.

### API Endpoints

| Method | Path | Description |
|---|---|---|
| GET | `/` | Built-in web dashboard (self-contained HTML page). |
| GET | `/health` | Liveness probe; returns `{"status":"ok"}`. |
| GET | `/metrics` | Prometheus metrics (stored txs, monitored accounts, uptime). |
| GET | `/accounts` | JSON list of the monitored accounts. |
| GET | `/transactions` | Stored transactions for a public key (see query params below). |
| GET | `/transactions/{signature}` | A single stored transaction by its signature. |
| GET | `/accounts/{pub_key}/balance` | Current lamport balance for an account, fetched live from the RPC. |
| POST | `/refresh` | Triggers an out-of-band fetch of recent transactions for the monitored key. |

#### `GET /transactions`

Retrieves stored transactions filtered by public key and optional date.

**Query parameters:**

- `pub_key`: The public key to fetch transactions for.
- `day` (optional): Filter transactions by a specific day in `dd/mm/yyyy` format.
- `limit` (optional): Limit the number of transactions returned (default is 5).
- `offset` (optional): Offset for pagination.

Example:

```
curl "http://127.0.0.1:3030/transactions?pub_key=YourPublicKeyHere"
```

**Response shape** — each transaction is decoded from the RPC response into an
enriched record (not a transfer-biased guess):

```json
{
  "signature": "5dWj3A…",
  "slot": 331457812,
  "timestamp": 1721812345,
  "fee": 5000,
  "fee_payer": "9xQe…",
  "success": true,
  "tx_type": "transfer",
  "programs": ["system"],
  "transfer": { "source": "9xQe…", "destination": "3Fh2…", "lamports": 1000000 },
  "token_changes": [
    { "mint": "EPjF…", "owner": "9xQe…", "change": "-1000000", "decimals": 6, "ui_change": -1.0 }
  ]
}
```

- `tx_type` classifies the transaction: `transfer` (native SOL), `vote`,
  `token` (SPL), the invoked program name, or `unknown`.
- `transfer` is present **only** for native SOL transfers, and its `lamports`
  is the *actual* amount parsed from the System Program instruction — including
  transfers made via **CPI/inner instructions** — not a balance-delta guess.
  Non-transfer transactions carry `null` here.
- `token_changes` lists net **SPL token** balance changes (mint, owner, signed
  amount, decimals), derived from the transaction's pre/post token balances — so
  it captures real token amounts regardless of how the transfer was structured
  (including CPI). Empty when no token balances moved.

## Project Structure

The project is organized into the following modules:

- aggregator.rs: Handles the logic for fetching transactions from the Solana blockchain.
- api.rs: Defines and implements the RESTful API for querying transactions.
- db.rs: Implements an in-memory database with the ability to persist transactions to a text file.
- main.rs: The entry point of the application. It initializes components, starts the server, and handles graceful shutdown.

## Persistence

The service supports two storage backends, selected at runtime:

**In-memory + file (default).** Transactions are held in memory and appended to a
text file (`transactions.txt`, JSONL, idempotent by signature), reloaded on
startup. Zero setup — good for local use and small datasets.

**Postgres (set `DATABASE_URL`).** Durable, indexed, and queryable — the
production path. On startup the app runs the migrations in `migrations/` and
stores each transaction in a `transactions` table keyed by `(account,
signature)` (idempotent via `ON CONFLICT DO NOTHING`), with the native SOL
transfer flattened into `transfer_source/destination/lamports` columns so it can
be filtered and summed in SQL.

A local Postgres is one command away via the included compose file:

```
docker compose up -d          # starts postgres:16 on :5432
export DATABASE_URL=postgres://solana:solana@localhost:5432/solana_aggregator
cargo run
```

The app logs `Using Postgres store` (vs `Using in-memory store`) at startup so
you can confirm which backend is active.

## Testing

The project includes a comprehensive set of unit tests to ensure the correctness of its core components:

- Database Tests: Verify that transactions are correctly added, retrieved, and persisted.
- API Tests: Check the functionality and correctness of the API endpoints.
- Aggregator Tests: Test the integration between transaction fetching and storage.

To run the tests:

```
cargo test
```

## Design Decisions

In-Memory Database with File Persistence: This design was chosen for its balance between performance and simplicity. The in-memory database allows for fast querying, while file persistence ensures data is not lost between sessions.
Timeouts for Data Fetching: To prevent the application from hanging if the Solana network is slow or unresponsive, timeouts are used when fetching transactions.

## Deployment

The service ships as a single static binary with the dashboard and migrations
embedded, so the container image is small and self-contained.

**Docker image** (multi-stage build, runs as a non-root user):

```
docker build -t solana-data-aggregator .
docker run -p 3030:3030 \
  -e SOLANA_RPC_URL=https://api.testnet.solana.com \
  -e SOLANA_PUBLIC_KEYS=Key1,Key2 \
  solana-data-aggregator
```

The image sets `SERVER_ADDR=0.0.0.0:3030` so the API is reachable from outside
the container.

**Full stack** (app + Postgres) via the `app` compose profile:

```
docker compose --profile app up -d --build
```

This starts Postgres and the aggregator wired to it (`DATABASE_URL` points at the
`postgres` service). Without `--profile app`, `docker compose up -d` starts only
Postgres — handy for local `cargo run`.

**Observability**: scrape `GET /metrics` (Prometheus format) and probe
`GET /health` for liveness.

**TLS & auth**: terminate TLS and enforce authentication at a reverse proxy
(nginx/Traefik/Caddy) in front of the service — the standard pattern — rather
than in the app. Keep the container on a private network and expose only the
proxy.

## Future Enhancements

Pair token balance changes into explicit transfers (source → destination per mint), and decode more instruction types (swaps, stakes) beyond native SOL and SPL token movements.
Horizontal scale & backfill: shard accounts across workers and backfill full history beyond the current-epoch window.

## Contributing

Contributions are welcome! If you have ideas for improvements or want to fix bugs, please open an issue or submit a pull request. Before contributing, please read our contribution guidelines.

## License

This project is licensed under the MIT License. For more details, see the LICENSE.md file.

## Acknowledgments

A big thank you to the Solana community for providing the tools and documentation that made this project possible.
Thanks to all the contributors who helped improve this project.
