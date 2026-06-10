FROM rust:1.88.0-slim-bookworm AS builder

RUN apt-get update && \
    apt-get install -y build-essential pkg-config libssl-dev perl

COPY . /app
WORKDIR /app
RUN cargo build --release

FROM debian:bookworm-slim
# `apt-get upgrade -y` pulls in the debian-security stream so Trivy
# `--severity HIGH,CRITICAL --ignore-unfixed` reports zero findings on the
# runtime image. Without it the base bookworm-slim digest ships libgnutls30
# 3.7.9-2+deb12u6 with five fixable HIGH/CRITICAL CVEs (33845 / 42010 /
# 33846 / 3833 / 42009). The fix is one apt step and only ~5 MiB.
RUN apt-get update && apt-get upgrade -y && apt-get install  -o Dpkg::Options::=--force-confdef -yq --no-install-recommends postgresql-client openssl \
    # Clean up layer
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/* \
    && truncate -s 0 /var/log/*log

# Run as non-root. Port 6432 does not require CAP_NET_BIND_SERVICE.
# The SIGUSR2 binary-upgrade fd-passing path operates entirely within the
# process and does not need root. /etc/pg_doorman is owned by pgdoorman so
# operator-mounted configs can be made read-only with the same uid/gid.
RUN groupadd --system --gid 999 pgdoorman && \
    useradd --system --uid 999 --gid 999 --no-create-home --shell /sbin/nologin pgdoorman && \
    mkdir -p /etc/pg_doorman && \
    chown -R pgdoorman:pgdoorman /etc/pg_doorman

COPY --from=builder /app/target/release/pg_doorman /usr/bin/pg_doorman
COPY --from=builder /app/target/release/patroni_proxy /usr/bin/patroni_proxy
WORKDIR /etc/pg_doorman
USER pgdoorman
ENV RUST_LOG=info
CMD ["pg_doorman"]
# SIGTERM for immediate shutdown in containers.
# SIGINT in non-TTY triggers binary upgrade (spawns child, PID 1 exits,
# container dies). SIGTERM avoids this.
STOPSIGNAL SIGTERM
