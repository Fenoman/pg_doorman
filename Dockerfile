FROM gcr.io/distroless/cc-debian13@sha256:9b615fff20e1a4fad29c2b30562580b212c7dd5e2225236735cca0070ed11c78 AS runtime-base

FROM rust:1.88.0-slim-trixie AS builder

RUN apt-get update && \
    apt-get install -y --no-install-recommends build-essential pkg-config libssl-dev perl

# Keep the complete Debian libc6 payload and its package inventory together.
# The pinned distroless release still contains u3; u4 fixes CVE-2026-5450 and
# CVE-2026-5928. Remove this overlay when updating to a base that includes u4.
ARG LIBC6_VERSION=2.41-12+deb13u4
RUN apt-get update && \
    mkdir -p /tmp/runtime-debs /runtime-root/var/lib/dpkg/status.d && \
    cd /tmp/runtime-debs && \
    apt-get download "libc6=${LIBC6_VERSION}" && \
    dpkg-deb --extract libc6_*.deb /runtime-root && \
    dpkg-deb --control libc6_*.deb /tmp/libc6-control && \
    cp /tmp/libc6-control/control /runtime-root/var/lib/dpkg/status.d/libc6 && \
    cp /tmp/libc6-control/md5sums /runtime-root/var/lib/dpkg/status.d/libc6.md5sums

# Embed the resolved Rust dependency inventory so image scanners cover both
# application binaries as well as the operating-system packages.
RUN cargo install cargo-auditable --version 0.7.5 --locked

COPY . /app
WORKDIR /app
RUN cargo auditable build --locked --release --bin pg_doorman --bin patroni_proxy

# The runtime stage is distroless and has no shell, so everything that used to
# be an in-image `RUN` has to be materialised here and copied in as files.
COPY --from=runtime-base /etc/passwd /distroless/passwd
COPY --from=runtime-base /etc/group /distroless/group

# Keep uid/gid 999 rather than adopting distroless' own `nonroot` (65532):
# operators already mount read-only configs owned by 999, and the daemon-mode
# `daemon.user`/`daemon.group` lookup goes through getpwnam(3), so the
# account has to exist by name as well as by number. The distroless entries
# are preserved so `root`/`nobody`/`nonroot` keep resolving.
RUN cp /distroless/passwd /rootfs-passwd && \
    cp /distroless/group /rootfs-group && \
    echo 'pgdoorman:x:999:999::/nonexistent:/sbin/nologin' >> /rootfs-passwd && \
    echo 'pgdoorman:x:999:' >> /rootfs-group && \
    install -d -m 0755 -o 999 -g 999 /rootfs-etc-pg_doorman

FROM runtime-base

COPY --from=builder /runtime-root/ /
COPY --from=builder /rootfs-passwd /etc/passwd
COPY --from=builder /rootfs-group /etc/group
COPY --from=builder --chown=999:999 /rootfs-etc-pg_doorman /etc/pg_doorman

COPY --from=builder /app/target/release/pg_doorman /usr/bin/pg_doorman
COPY --from=builder /app/target/release/patroni_proxy /usr/bin/patroni_proxy
WORKDIR /etc/pg_doorman
# Run as non-root. Port 6432 does not require CAP_NET_BIND_SERVICE.
# The SIGUSR2 binary-upgrade fd-passing path operates entirely within the
# process and does not need root. Numeric form so the USER directive keeps
# satisfying `runAsNonRoot` even if /etc/passwd is replaced at deploy time.
USER 999:999
ENV RUST_LOG=info
CMD ["pg_doorman"]
# SIGTERM for immediate shutdown in containers.
# SIGINT in non-TTY triggers binary upgrade (spawns child, PID 1 exits,
# container dies). SIGTERM avoids this.
STOPSIGNAL SIGTERM
