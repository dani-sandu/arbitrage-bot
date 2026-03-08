# Build stage — must match deploy stage Debian version to avoid GLIBC mismatch
FROM rust:bookworm AS builder

WORKDIR /app

COPY . .
RUN cargo build --release

# Deploy stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates procps && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/arb-rust /app/arb-rust

ENV RUST_LOG=info

CMD ["./arb-rust"]
