# syntax=docker/dockerfile:1

FROM rust:1.87.0-slim-bookworm AS builder

RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential pkg-config libssl-dev perl ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY . .

RUN cargo build --release --locked

RUN set -eux; \
    mkdir -p /runtime-libs; \
    for lib in $(find /lib /usr/lib -type f -name 'libgcc_s.so.1'); do \
      mkdir -p "/runtime-libs$(dirname "$lib")"; \
      cp "$lib" "/runtime-libs$lib"; \
    done

FROM gcr.io/distroless/base-nossl-debian12:nonroot
LABEL org.opencontainers.image.base.name="gcr.io/distroless/base-nossl-debian12:nonroot"

COPY --from=builder /app/target/release/pg_doorman /usr/bin/pg_doorman
COPY --from=builder /app/target/release/patroni_proxy /usr/bin/patroni_proxy
COPY --from=builder /runtime-libs/ /

EXPOSE 6432
WORKDIR /etc/pg_doorman
ENV RUST_LOG=info
USER nonroot
ENTRYPOINT ["/usr/bin/pg_doorman"]
# SIGTERM for immediate shutdown in containers.
# SIGINT in non-TTY triggers binary upgrade (spawns child, PID 1 exits,
# container dies). SIGTERM avoids this.
STOPSIGNAL SIGTERM
