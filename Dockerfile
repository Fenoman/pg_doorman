# syntax=docker/dockerfile:1

FROM rust:1.87.0-slim-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config perl ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

ENV OPENSSL_STATIC=1 \
    OPENSSL_NO_VENDOR=0

COPY . .

RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /app/target/release/pg_doorman /usr/bin/pg_doorman

WORKDIR /etc/pg_doorman
ENV RUST_LOG=info
USER nonroot
ENTRYPOINT ["/usr/bin/pg_doorman"]
STOPSIGNAL SIGINT
