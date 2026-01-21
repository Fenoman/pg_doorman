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

FROM gcr.io/distroless/cc-debian12:nonroot@sha256:dc65e8ce812dac0f34ca456729ba0cb8a7c1b7c71078be099fb12390a33c4c31

COPY --from=builder /app/target/release/pg_doorman /usr/bin/pg_doorman
COPY --from=builder /app/target/release/patroni_proxy /usr/bin/patroni_proxy

WORKDIR /etc/pg_doorman
ENV RUST_LOG=info
USER nonroot
ENTRYPOINT ["/usr/bin/pg_doorman"]
STOPSIGNAL SIGINT
