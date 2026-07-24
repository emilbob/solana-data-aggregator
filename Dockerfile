# ---- build stage ----
FROM rust:1-slim AS builder
WORKDIR /app

# Build dependencies first (cached) using a stub main, then the real sources.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo "fn main() {}" > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY migrations ./migrations
COPY src ./src
# Bust the cached stub build so the real binary is compiled.
RUN touch src/main.rs && cargo build --release --locked

# ---- runtime stage ----
FROM debian:bookworm-slim
# CA certificates are needed for TLS to the RPC endpoint and Postgres.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
# Run as a non-root user.
RUN useradd --system --uid 10001 appuser
COPY --from=builder /app/target/release/solana-data-aggregator /usr/local/bin/solana-data-aggregator
USER appuser
# Bind to all interfaces inside the container (override host:port via SERVER_ADDR).
ENV SERVER_ADDR=0.0.0.0:3030
EXPOSE 3030
ENTRYPOINT ["/usr/local/bin/solana-data-aggregator"]
