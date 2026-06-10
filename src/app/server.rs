use std::net::{SocketAddr, ToSocketAddrs};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use log::{debug, error, info, warn};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpSocket;
#[cfg(not(windows))]
use tokio::signal::unix::{signal as unix_signal, SignalKind};
#[cfg(windows)]
use tokio::signal::windows as win_signal;
use tokio::sync::Notify;
use tokio::{runtime::Builder, sync::mpsc};

use crate::app::args::Args;
use crate::config::{config_arc, get_config, reload_config, Config};
use crate::daemon;
use crate::messages::{configure_tcp_socket, configure_unix_socket};
use crate::pool::{retain, ClientServerMap, ConnectionPool};
use crate::server::{gc_sweep_anon, gc_sweep_named};
use crate::stats::{Collector, Reporter, REPORTER, TOTAL_CONNECTION_COUNTER};
use crate::utils::core_affinity;
use crate::utils::format_duration;
use crate::web::metrics::record_interner_gc;
use crate::web::WebServerOptions;
use socket2::SockRef;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
#[cfg(not(windows))]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(not(windows))]
use std::os::unix::process::CommandExt;

use crate::app::tls::init_tls;
#[cfg(unix)]
use crate::client::migration::{migration_receiver_task, migration_sender_task};
use crate::client::migration::{MigrationPayload, MAX_MIGRATION_PAYLOAD_BYTES};

#[cfg(not(windows))]
const DAEMON_PID_FILE_FD_ENV: &str = "PG_DOORMAN_DAEMON_PID_FD";
#[cfg(not(windows))]
const DAEMON_IDENTITY_FD_ENV: &str = "PG_DOORMAN_DAEMON_IDENTITY_FD";

/// Global counter for clients currently connected to the pg_doorman
pub static CURRENT_CLIENT_COUNT: AtomicI64 = AtomicI64::new(0);

/// RAII guard for `CURRENT_CLIENT_COUNT`. The guard keeps the gauge balanced
/// during unwinding, early returns, and task drops so `max_connections`
/// enforcement and graceful-shutdown draining observe the real live-client
/// count.
pub struct ClientCountGuard;

impl ClientCountGuard {
    /// Acquire - increments the counter and returns the guard.
    #[inline]
    pub fn acquire() -> Self {
        CURRENT_CLIENT_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self
    }
}

impl Drop for ClientCountGuard {
    #[inline]
    fn drop(&mut self) {
        CURRENT_CLIENT_COUNT.fetch_add(-1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Unix-epoch second of the last accept-loop EMFILE/ENFILE log.
static ACCEPT_RESOURCE_LOG_LAST: AtomicI64 = AtomicI64::new(0);

/// Global flag indicating graceful shutdown is in progress
pub static SHUTDOWN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

/// readiness gate for k8s `/ready` probes. Flipped to
/// `true` AFTER (a) `ConnectionPool::from_config` returned Ok at
/// startup AND (b) the main PG listener bound and is accepting. /ready
/// returns 503 until both are true - prevents k8s from routing client
/// traffic to a pod that's still spawning pools (which earlier
/// caused "connection refused" flap during cold start).
///
/// The shutdown side (`/ready` -> 503 when `SHUTDOWN_IN_PROGRESS == true`)
/// startup also follows this readiness rule.
pub static READY: AtomicBool = AtomicBool::new(false);

/// Global counter for clients currently in transactions (holding server connections)
pub static CLIENTS_IN_TRANSACTIONS: AtomicI64 = AtomicI64::new(0);

/// Global flag: migration to new process is active. Clients should self-migrate at idle points.
pub static MIGRATION_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

#[inline]
pub fn publish_migration_in_progress(in_progress: bool) {
    MIGRATION_IN_PROGRESS.store(in_progress, Ordering::Release);
}

#[inline]
pub fn migration_in_progress() -> bool {
    MIGRATION_IN_PROGRESS.load(Ordering::Acquire)
}

/// Absolute deadline by which a self-migrating client must obtain a migration
/// channel slot. Published once, in the same block that creates `MIGRATION_TX`,
/// as `now + shutdown_timeout`. Clients wait for a slot up to this instant
/// (instead of being dropped on the first full-channel poll) and give up
/// gracefully if it elapses - so an unreachable/stuck successor can never hang
/// a client past the parent's shutdown window. Set once per process lifetime
/// (a process migrates out exactly once, on its own SIGUSR2).
pub static MIGRATION_DEADLINE: std::sync::OnceLock<tokio::time::Instant> =
    std::sync::OnceLock::new();

#[inline]
pub fn migration_deadline() -> Option<tokio::time::Instant> {
    MIGRATION_DEADLINE.get().copied()
}

/// Wakes idle client tasks after the new process is ready to receive migrated fds.
#[cfg(unix)]
pub static MIGRATION_NOTIFY: std::sync::LazyLock<Notify> = std::sync::LazyLock::new(Notify::new);

const MIGRATION_FRESH_ACCEPT_GRACE: Duration = Duration::from_millis(250);

async fn wait_for_migration_receiver_drain(
    migration_receiver_active: &AtomicBool,
    migration_fresh_accept_released: &AtomicBool,
    migration_receiver_drained: &Notify,
) {
    while migration_receiver_active.load(Ordering::Acquire)
        && !migration_fresh_accept_released.load(Ordering::Acquire)
    {
        if tokio::time::timeout(
            MIGRATION_FRESH_ACCEPT_GRACE,
            migration_receiver_drained.notified(),
        )
        .await
        .is_err()
        {
            migration_fresh_accept_released.store(true, Ordering::Release);
            migration_receiver_drained.notify_waiters();
            debug!(
                "migration receiver still active after {MIGRATION_FRESH_ACCEPT_GRACE:?}; releasing fresh accepts"
            );
        }
    }
}

/// Process start time for API uptime reporting.
pub static STARTED_AT: std::sync::LazyLock<std::time::SystemTime> =
    std::sync::LazyLock::new(std::time::SystemTime::now);

/// `STARTED_AT` rendered as Unix epoch milliseconds.
pub static STARTED_AT_MS: std::sync::LazyLock<u64> = std::sync::LazyLock::new(|| {
    STARTED_AT
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
});

/// Channel sender for migration payloads. Set once when migration starts.
pub static MIGRATION_TX: std::sync::OnceLock<mpsc::Sender<MigrationPayload>> =
    std::sync::OnceLock::new();

/// Hard cap for queued migration fd duplicates.
const MIGRATION_CHANNEL_FD_CAPACITY_MAX: usize = 4096;
/// Worst-case heap budget for queued serialized migration payloads.
const MIGRATION_QUEUED_PAYLOAD_HEAP_BUDGET_BYTES: usize = 512 * 1024 * 1024;
const MIGRATION_CHANNEL_CAPACITY_BY_HEAP: usize =
    MIGRATION_QUEUED_PAYLOAD_HEAP_BUDGET_BYTES / MAX_MIGRATION_PAYLOAD_BYTES;
const MIGRATION_CHANNEL_HEAP_CAPACITY_MAX: usize = if MIGRATION_CHANNEL_CAPACITY_BY_HEAP == 0 {
    1
} else {
    MIGRATION_CHANNEL_CAPACITY_BY_HEAP
};
const MIGRATION_CHANNEL_CAPACITY_MAX: usize =
    if MIGRATION_CHANNEL_HEAP_CAPACITY_MAX < MIGRATION_CHANNEL_FD_CAPACITY_MAX {
        MIGRATION_CHANNEL_HEAP_CAPACITY_MAX
    } else {
        MIGRATION_CHANNEL_FD_CAPACITY_MAX
    };
const MIGRATION_SENDER_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(unix)]
const INHERITED_UNIX_SOCKET_DEV_ENV: &str = "PG_DOORMAN_INHERIT_UNIX_SOCKET_DEV";
#[cfg(unix)]
const INHERITED_UNIX_SOCKET_INO_ENV: &str = "PG_DOORMAN_INHERIT_UNIX_SOCKET_INO";

/// Parent-side fd reserve for spawn, readiness, and migration drain work.
const MIGRATION_SPAWN_RESERVE_FDS: u64 = 16;

/// Live fd count for sizing the SIGUSR2 migration queue.
/// Returns `None` outside Linux so callers can fall back conservatively.
#[cfg(target_os = "linux")]
fn count_open_fds() -> Option<u64> {
    std::fs::read_dir("/proc/self/fd")
        .ok()
        .map(|entries| entries.count() as u64)
}

#[cfg(not(target_os = "linux"))]
fn count_open_fds() -> Option<u64> {
    None
}

/// Capacity for dup'd client fds waiting on the migration socket.
/// Returns `None` when there is no safe headroom under `RLIMIT_NOFILE`.
#[cfg(unix)]
fn safe_migration_capacity() -> Option<usize> {
    // SAFETY: getrlimit writes only to the stack-local rlimit struct.
    let soft_limit = unsafe {
        let mut rl: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) == 0 {
            rl.rlim_cur
        } else {
            MIGRATION_CHANNEL_CAPACITY_MAX as u64
        }
    };
    // No /proc/self/fd: assume half the limit is already in use.
    let open_fds = count_open_fds().unwrap_or(soft_limit / 2);
    let headroom = soft_limit
        .saturating_sub(open_fds)
        .saturating_sub(MIGRATION_SPAWN_RESERVE_FDS);
    if headroom == 0 {
        return None;
    }
    Some((headroom as usize).clamp(1, MIGRATION_CHANNEL_CAPACITY_MAX))
}

#[cfg(not(unix))]
fn safe_migration_capacity() -> Option<usize> {
    Some(MIGRATION_CHANNEL_CAPACITY_MAX)
}

fn live_shutdown_timeout() -> Duration {
    get_config().general.shutdown_timeout.as_std()
}

/// Minimum gap between two consecutive accept-loop EMFILE/ENFILE log lines.
const ACCEPT_RESOURCE_LOG_INTERVAL_SECS: i64 = 5;

/// Accept/spawn failed because the process or host fd table is exhausted.
#[cfg(unix)]
fn is_fd_exhaustion_io(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EMFILE) | Some(libc::ENFILE),)
}

#[cfg(not(windows))]
fn set_fd_close_on_exec(fd: libc::c_int, label: &str) {
    // SAFETY: fcntl reads and writes descriptor flags for the supplied fd only.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            warn!(
                "Failed to read close-on-exec flag for {label} fd={fd}: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        if libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
            warn!(
                "Failed to set close-on-exec flag for {label} fd={fd}: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

#[cfg(not(windows))]
fn set_fd_nonblocking(fd: libc::c_int, label: &str) {
    // SAFETY: fcntl reads and writes descriptor status flags for the supplied fd only.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            warn!(
                "Failed to read nonblocking flag for {label} fd={fd}: {}",
                std::io::Error::last_os_error()
            );
            return;
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            warn!(
                "Failed to set nonblocking flag for {label} fd={fd}: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

#[cfg(not(windows))]
fn getsockopt_int(fd: libc::c_int, opt: libc::c_int) -> std::io::Result<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt writes an int into `value` and updates `len`.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            opt,
            &mut value as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(value)
}

#[cfg(not(windows))]
fn validate_inherited_stream_socket_fd(fd: libc::c_int) -> std::io::Result<()> {
    if fd < 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid inherited listener fd {fd}"),
        ));
    }
    // SAFETY: fcntl(F_GETFD) only validates descriptor table state.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let sock_type = getsockopt_int(fd, libc::SO_TYPE)?;
    if sock_type != libc::SOCK_STREAM {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("inherited fd {fd} is not a stream socket"),
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn validate_inherited_listener_fd(fd: libc::c_int) -> std::io::Result<()> {
    validate_inherited_stream_socket_fd(fd)?;
    let accepting = getsockopt_int(fd, libc::SO_ACCEPTCONN)?;
    if accepting == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("inherited fd {fd} is not a listening socket"),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_inherited_unix_listener_fd(fd: libc::c_int) -> std::io::Result<()> {
    validate_inherited_stream_socket_fd(fd)?;
    match getsockopt_int(fd, libc::SO_ACCEPTCONN) {
        Ok(0) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("inherited Unix fd {fd} is not a listening socket"),
        )),
        Ok(_) => Ok(()),
        Err(err) if err.raw_os_error() == Some(libc::ENOPROTOOPT) => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
fn unix_fd_mode(fd: libc::c_int) -> std::io::Result<u32> {
    let mut stat_buf = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstat reads metadata for the supplied descriptor only.
    let rc = unsafe { libc::fstat(fd, stat_buf.as_mut_ptr()) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stat_buf = unsafe { stat_buf.assume_init() };
    // st_mode is u16 on macOS and u32 on Linux; the cast is required for the
    // former and a no-op for the latter.
    #[allow(clippy::unnecessary_cast)]
    Ok((stat_buf.st_mode as u32) & 0o777)
}

#[cfg(unix)]
fn ensure_unix_path_matches_ownership(
    path: &str,
    expected_dev: u64,
    expected_ino: u64,
) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let meta = std::fs::symlink_metadata(path)?;
    let path_dev = meta.dev();
    let path_ino = meta.ino();
    if path_dev != expected_dev || path_ino != expected_ino {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "Unix socket path {path} no longer names the inherited listener socket \
                 (path dev={path_dev} ino={path_ino}, expected dev={expected_dev} \
                 ino={expected_ino})"
            ),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn set_unix_fd_mode(fd: libc::c_int, mode: u32) -> std::io::Result<()> {
    // SAFETY: fchmod changes permissions on the supplied descriptor's inode.
    let rc = unsafe { libc::fchmod(fd, mode as libc::mode_t) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn drop_listener_owner(listener: &mut Option<tokio::net::TcpListener>) {
    drop(listener.take());
}

#[cfg(unix)]
fn drop_unix_listener_owner(listener: &mut Option<tokio::net::UnixListener>) {
    drop(listener.take());
}

#[cfg(not(windows))]
fn adopt_inherited_tcp_listener(
    fd: libc::c_int,
    expected_addr: SocketAddr,
) -> std::io::Result<tokio::net::TcpListener> {
    validate_inherited_stream_socket_fd(fd)?;
    // SAFETY: validation above proved fd is an open stream socket. The
    // listening-state check runs after local_addr validation so a wrong
    // listener gets a precise diagnostic.
    let std_listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
    let actual_addr = std_listener.local_addr()?;
    if actual_addr != expected_addr {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("inherited fd {fd} listens on {actual_addr}, expected {expected_addr}"),
        ));
    }
    validate_inherited_listener_fd(std_listener.as_raw_fd())?;
    set_fd_close_on_exec(std_listener.as_raw_fd(), "inherited listener");
    std_listener.set_nonblocking(true)?;
    tokio::net::TcpListener::from_std(std_listener)
}

#[cfg(unix)]
fn adopt_inherited_unix_listener(
    fd: libc::c_int,
    expected_path: &str,
    _mode: u32,
    expected_ownership: Option<(u64, u64)>,
) -> std::io::Result<tokio::net::UnixListener> {
    validate_inherited_unix_listener_fd(fd)?;
    // SAFETY: validation above proved fd is an open listening stream socket.
    let std_listener = unsafe { std::os::unix::net::UnixListener::from_raw_fd(fd) };
    let actual_addr = std_listener.local_addr()?;
    match actual_addr.as_pathname() {
        Some(path) if path == std::path::Path::new(expected_path) => {}
        Some(path) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "inherited Unix fd {fd} listens on {}, expected {expected_path}",
                    path.display()
                ),
            ));
        }
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("inherited Unix fd {fd} is unnamed, expected {expected_path}"),
            ));
        }
    }
    // Do not chmod `expected_path` here: the pathname can be replaced after
    // fd validation. The parent pre-upgrade path tightens the inherited
    // listener before spawn, or aborts the upgrade if that cannot be done via
    // the listener fd.
    if let Some((expected_dev, expected_ino)) = expected_ownership {
        ensure_unix_path_matches_ownership(expected_path, expected_dev, expected_ino)?;
    }
    set_fd_close_on_exec(std_listener.as_raw_fd(), "inherited Unix listener");
    std_listener.set_nonblocking(true)?;
    tokio::net::UnixListener::from_std(std_listener)
}

#[cfg(not(windows))]
pub fn cleanup_inherited_upgrade_fds(args: &Args) {
    let Some(mut keep) = inherited_upgrade_fd_allowlist(args) else {
        return;
    };
    keep.sort_unstable();
    keep.dedup();

    let closed = close_unexpected_fds_below_limit(&keep);
    if closed > 0 {
        eprintln!(
            "binary upgrade: closed {closed} unexpected inherited file descriptor(s) before startup"
        );
    }
}

#[cfg(windows)]
pub fn cleanup_inherited_upgrade_fds(_args: &Args) {}

#[cfg(not(windows))]
fn inherited_upgrade_fd_allowlist(args: &Args) -> Option<Vec<libc::c_int>> {
    let forced_cleanup = std::env::var("PG_DOORMAN_CLOSE_INHERITED_FDS")
        .map(|value| value == "1")
        .unwrap_or(false);
    if !forced_cleanup && args.inherit_fd.is_none() && args.inherit_unix_fd.is_none() {
        return None;
    }
    std::env::remove_var("PG_DOORMAN_CLOSE_INHERITED_FDS");

    let mut keep = vec![0, 1, 2];

    if let Some(listener_fd) = args.inherit_fd {
        keep.push(listener_fd);
    }
    if let Some(unix_listener_fd) = args.inherit_unix_fd {
        keep.push(unix_listener_fd);
    }

    if let Some(fd) = parse_fd_env("PG_DOORMAN_READY_FD") {
        keep.push(fd);
    }
    if let Some(fd) = parse_fd_env("PG_DOORMAN_MIGRATION_FD") {
        keep.push(fd);
    }
    if let Some(fd) = parse_fd_env(DAEMON_PID_FILE_FD_ENV) {
        keep.push(fd);
    }
    if let Some(fd) = parse_fd_env(DAEMON_IDENTITY_FD_ENV) {
        keep.push(fd);
    }

    Some(keep)
}

#[cfg(not(windows))]
fn parse_fd_env(name: &str) -> Option<libc::c_int> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<libc::c_int>().ok())
        .filter(|fd| *fd >= 0)
}

#[cfg(unix)]
fn parse_inherited_unix_socket_ownership_env() -> Result<Option<(u64, u64)>, String> {
    let dev = std::env::var(INHERITED_UNIX_SOCKET_DEV_ENV).ok();
    let ino = std::env::var(INHERITED_UNIX_SOCKET_INO_ENV).ok();
    std::env::remove_var(INHERITED_UNIX_SOCKET_DEV_ENV);
    std::env::remove_var(INHERITED_UNIX_SOCKET_INO_ENV);

    match (dev, ino) {
        (None, None) => Ok(None),
        (Some(dev), Some(ino)) => {
            let dev = dev.parse::<u64>().map_err(|err| {
                format!("invalid {INHERITED_UNIX_SOCKET_DEV_ENV} value {dev:?}: {err}")
            })?;
            let ino = ino.parse::<u64>().map_err(|err| {
                format!("invalid {INHERITED_UNIX_SOCKET_INO_ENV} value {ino:?}: {err}")
            })?;
            Ok(Some((dev, ino)))
        }
        _ => Err(format!(
            "{INHERITED_UNIX_SOCKET_DEV_ENV} and {INHERITED_UNIX_SOCKET_INO_ENV} must be set together"
        )),
    }
}

#[cfg(not(windows))]
fn close_unexpected_fds_below_limit(keep: &[libc::c_int]) -> usize {
    #[cfg(target_os = "linux")]
    if let Some(fds) = open_fds_from_proc() {
        return close_unexpected_fds(fds, keep);
    }

    let upper = fd_cleanup_upper_bound();
    close_unexpected_fds(3..upper, keep)
}

#[cfg(target_os = "linux")]
fn open_fds_from_proc() -> Option<Vec<libc::c_int>> {
    let entries = std::fs::read_dir("/proc/self/fd").ok()?;
    let mut fds = Vec::new();
    for entry in entries.flatten() {
        if let Ok(fd) = entry.file_name().to_string_lossy().parse::<libc::c_int>() {
            fds.push(fd);
        }
    }
    Some(fds)
}

#[cfg(not(windows))]
fn close_unexpected_fds<I>(fds: I, keep: &[libc::c_int]) -> usize
where
    I: IntoIterator<Item = libc::c_int>,
{
    let mut closed = 0usize;

    for fd in fds {
        if fd <= 2 {
            continue;
        }
        if keep.binary_search(&fd).is_ok() {
            continue;
        }
        // SAFETY: runs during process startup before Tokio is initialized.
        // EBADF means the slot was empty; cleanup ignores it.
        if unsafe { libc::close(fd) } == 0 {
            closed += 1;
        }
    }

    closed
}

#[cfg(not(windows))]
fn fd_cleanup_upper_bound() -> libc::c_int {
    // SAFETY: getrlimit writes to the stack-local rlimit struct only.
    unsafe {
        let mut rl: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) == 0 {
            return rl.rlim_cur.min(libc::c_int::MAX as u64) as libc::c_int;
        }
    }
    65_536
}

/// Rate-limit accept-loop fd-exhaustion logs without moving the window
/// on suppressed attempts.
fn should_log_accept_resource_now() -> bool {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let last = ACCEPT_RESOURCE_LOG_LAST.load(Ordering::Relaxed);
    now_secs.saturating_sub(last) >= ACCEPT_RESOURCE_LOG_INTERVAL_SECS
        && ACCEPT_RESOURCE_LOG_LAST
            .compare_exchange(last, now_secs, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
}

fn select_worker_affinity_core(
    core_ids: &[core_affinity::CoreId],
    ticket: usize,
) -> Option<core_affinity::CoreId> {
    if core_ids.len() < 3 {
        None
    } else {
        Some(core_ids[ticket % core_ids.len()])
    }
}

pub fn run_server(args: Args, config: Config) -> Result<(), Box<dyn std::error::Error>> {
    if args.daemon && std::env::var("NOTIFY_SOCKET").is_ok() {
        warn!(
            "--daemon is incompatible with systemd Type=notify. \
             Remove --daemon from ExecStart or switch to Type=forking."
        );
    }
    // loud warning when `--daemon` is
    // used without `syslog_prog_name`. Daemonize redirects
    // stdout/stderr to /dev/null and the logger is bound to stderr
    // in `init_logging` - without syslog forwarding all log output
    // (including boot errors and PROCESS_START audit) is dropped on
    // /dev/null for the process lifetime. We keep this as a stderr
    // warning rather than an exit so legacy operators (and the
    // existing BDD test that exercises daemon mode without syslog)
    // continue to work - the warning is the only thing visible BEFORE
    // daemonize().
    if args.daemon && config.general.syslog_prog_name.is_none() {
        eprintln!(
            "WARNING: --daemon without general.syslog_prog_name - all log \
             output (boot errors, PROCESS_START audit, runtime warnings) \
             will be dropped on /dev/null after daemonize. Set \
             `general.syslog_prog_name = \"pg_doorman\"` to keep logs in \
             syslog/journald."
        );
    }
    if args.daemon {
        let pid_file = config.general.daemon_pid_file.clone();
        let inherited_pid_file_fd = if args.inherit_fd.is_some() {
            parse_fd_env(DAEMON_PID_FILE_FD_ENV)
        } else {
            None
        };
        std::env::remove_var(DAEMON_PID_FILE_FD_ENV);
        let daemon_identity_fd = parse_fd_env(DAEMON_IDENTITY_FD_ENV);
        std::env::remove_var(DAEMON_IDENTITY_FD_ENV);

        let mut daemonize = daemon::lib::Daemonize::new()
            .pid_file(pid_file)
            .working_directory(std::env::current_dir().unwrap())
            .chown_pid_file(true);
        if let Some(pid_file_fd) = inherited_pid_file_fd {
            daemonize = daemonize.inherited_pid_file_fd(pid_file_fd);
        }
        match daemonize.start() {
            Ok(_) => println!("Success, daemonized"),
            Err(e) => {
                eprintln!("Error daemonize: {e}");
                process::exit(exitcode::OSERR);
            }
        }
        if let Some(fd) = daemon_identity_fd {
            let pid = std::process::id();
            let payload = format!("{pid}\n");
            let rc =
                unsafe { libc::write(fd, payload.as_ptr() as *const libc::c_void, payload.len()) };
            if rc < 0 {
                warn!(
                    "[binary-upgrade] failed to publish daemon successor identity fd={fd}: {}",
                    std::io::Error::last_os_error()
                );
            }
            unsafe {
                libc::close(fd);
            }
        }
    }

    let tls_state = init_tls(&config);

    let thread_id = AtomicUsize::new(0);
    let core_ids = if config.general.worker_cpu_affinity_pinning {
        core_affinity::get_core_ids().unwrap_or_default()
    } else {
        Vec::new()
    };
    let mut worker_cpu_affinity_pinning =
        config.general.worker_cpu_affinity_pinning && core_ids.len() >= 3;
    if worker_cpu_affinity_pinning {
        let ticket = thread_id.fetch_add(1, Ordering::SeqCst);
        if let Some(core_id) = select_worker_affinity_core(&core_ids, ticket) {
            if !core_affinity::set_for_current(core_id) {
                warn!(
                    "Failed to pin main thread to core {}; disabling worker CPU affinity pinning",
                    core_id.id
                );
                worker_cpu_affinity_pinning = false;
            }
        }
    }

    let mut runtime_builder = Builder::new_multi_thread();
    runtime_builder
        .worker_threads(config.general.worker_threads)
        .enable_all()
        .thread_name("worker-pg-doorman");

    // Apply optional tokio runtime parameters only if explicitly configured.
    // Modern tokio versions handle defaults well, so these are optional.
    if let Some(interval) = config.general.tokio_global_queue_interval {
        runtime_builder.global_queue_interval(interval);
    }
    if let Some(interval) = config.general.tokio_event_interval {
        runtime_builder.event_interval(interval);
    }
    if let Some(ref stack_size) = config.general.worker_stack_size {
        runtime_builder.thread_stack_size(stack_size.as_usize());
    }
    if let Some(max_threads) = config.general.max_blocking_threads {
        runtime_builder.max_blocking_threads(max_threads);
    }

    let runtime = runtime_builder
        .on_thread_start(move || {
            if worker_cpu_affinity_pinning {
                let core_id = thread_id.fetch_add(1, Ordering::SeqCst);
                if let Some(worker_core) = select_worker_affinity_core(&core_ids, core_id) {
                    info!(
                        "Pinning tokio worker thread {} to core {}",
                        core_id, worker_core.id
                    );
                    if !core_affinity::set_for_current(worker_core) {
                        warn!(
                            "Failed to pin tokio worker thread {} to core {}",
                            core_id, worker_core.id
                        );
                    }
                }
            }
        })
        .build()?;

    // Store inherit_fd before moving args into runtime
    #[cfg(not(windows))]
    let inherit_fd = args.inherit_fd;
    #[cfg(unix)]
    let inherit_unix_fd = args.inherit_unix_fd;
    #[cfg(unix)]
    let inherit_unix_socket_ownership = match parse_inherited_unix_socket_ownership_env() {
        Ok(ownership) => ownership,
        Err(err) => {
            error!("Invalid inherited Unix socket ownership metadata: {err}");
            std::process::exit(exitcode::CONFIG);
        }
    };

    runtime.block_on(async move {
        // install signal handlers AS THE FIRST action
        // inside the tokio runtime, BEFORE listener bind / from_config
        // / web bind / readiness pipe. Previously these were installed
        // ~200 lines later (after the slow `from_config`), so a
        // SIGTERM that systemd sent during a slow boot (many pools,
        // unresponsive backend stalling on connect_timeout, TLS
        // handshake) killed the process with the kernel default
        // disposition - no Unix-socket cleanup, no migration drain,
        // no PROCESS_STOP log line. Tokio buffers the signal in the
        // handler's internal channel until the accept loop's
        // `tokio::select!` polls it, so an early-arriving SIGTERM
        // still produces an orderly exit.
        #[cfg(windows)]
        let mut term_signal = win_signal::ctrl_close().unwrap();
        #[cfg(windows)]
        let mut interrupt_signal = win_signal::ctrl_c().unwrap();
        #[cfg(windows)]
        let mut sighup_signal = win_signal::ctrl_shutdown().unwrap();
        #[cfg(not(windows))]
        let mut term_signal = unix_signal(SignalKind::terminate()).unwrap();
        #[cfg(not(windows))]
        let mut interrupt_signal = unix_signal(SignalKind::interrupt()).unwrap();
        #[cfg(not(windows))]
        let mut sighup_signal = unix_signal(SignalKind::hangup()).unwrap();
        // SIGUSR2 for binary upgrade (unix only; on windows this future never resolves)
        #[cfg(not(windows))]
        let mut upgrade_signal = unix_signal(SignalKind::user_defined2()).unwrap();

        // starting listener.
        let addr = format!("{}:{}", config.general.host, config.general.port)
            .to_socket_addrs()
            .unwrap()
            .next()
            .unwrap();

        #[cfg(not(windows))]
        let listener = if let Some(fd) = inherit_fd {
            // Inherit listener from parent process (binary upgrade in foreground mode)
            info!("Inheriting listener from parent process (fd={fd})");
            match adopt_inherited_tcp_listener(fd, addr) {
                Ok(listener) => listener,
                Err(err) => {
                    error!("Invalid inherited listener fd={fd}: {err}");
                    std::process::exit(exitcode::CONFIG);
                }
            }
        } else {
            // Create new listener
            let listen_socket = if addr.is_ipv4() {
                TcpSocket::new_v4().unwrap()
            } else {
                TcpSocket::new_v6().unwrap()
            };
            listen_socket
                .set_reuseaddr(true)
                .expect("can't set reuseaddr");
            listen_socket
                .set_reuseport(true)
                .expect("can't set reuseport");
            listen_socket
                .set_nodelay(true)
                .expect("can't set nodelay");
            {
                let sock_ref = SockRef::from(&listen_socket);
                sock_ref.set_linger(Some(Duration::from_secs(0)))
                    .expect("could not configure tcp_so_linger for socket");
            }
            // IPTOS_LOWDELAY: u8 = 0x10;
            if addr.is_ipv4() {
                match listen_socket.set_tos_v4(0x10) {
                    Ok(_) => (),
                    Err(err) => {
                        warn!("Failed to set IPTOS_LOWDELAY on listener socket: {err}");
                    }
                };
            };
            listen_socket.bind(addr).expect("can't bind");
            // end configure listener.
            let backlog = if config.general.backlog > 0 {
                config.general.backlog
            } else {
                config.general.max_connections as u32
            };
            match listen_socket.listen(backlog) {
                Ok(sock) => sock,
                Err(err) => {
                    error!("Listener socket error: {err}");
                    std::process::exit(exitcode::CONFIG);
                }
            }
        };

        #[cfg(windows)]
        let listener = {
            let listen_socket = if addr.is_ipv4() {
                TcpSocket::new_v4().unwrap()
            } else {
                TcpSocket::new_v6().unwrap()
            };
            listen_socket
                .set_reuseaddr(true)
                .expect("can't set reuseaddr");
            listen_socket
                .set_reuseport(true)
                .expect("can't set reuseport");
            listen_socket
                .set_nodelay(true)
                .expect("can't set nodelay");
            listen_socket
                .set_linger(Some(Duration::from_secs(0)))
                .expect("can't set linger 0");
            listen_socket.bind(addr).expect("can't bind");
            let backlog = if config.general.backlog > 0 {
                config.general.backlog
            } else {
                config.general.max_connections as u32
            };
            match listen_socket.listen(backlog) {
                Ok(sock) => sock,
                Err(err) => {
                    error!("Listener socket error: {err}");
                    std::process::exit(exitcode::CONFIG);
                }
            }
        };

        // do NOT set READY here - pools are NOT yet
        // loaded (from_config runs later, after this listener bind).
        // moved to after `from_config(...)` returns Ok so
        // /ready reflects actual cold-start readiness.
        info!("Running on {addr}");

        // Unix socket listener (when unix_socket_dir is set).
        //
        // Delegated to `create_unix_listener` so tests can exercise the
        // bind/chmod/ownership pipeline in a tempdir. `unix_socket_ownership`
        // captures the (dev, ino) of the inode we create here so the
        // shutdown path can tell our socket apart from one bound by a
        // successor process during a SIGUSR2 binary upgrade.
        let (mut unix_listener, mut unix_socket_ownership) = match config.general.unix_socket_dir {
            Some(ref dir) => {
                let path = format!("{dir}/.s.PGSQL.{}", config.general.port);
                let mode = crate::config::General::parse_unix_socket_mode(
                    &config.general.unix_socket_mode,
                )
                .expect("unix_socket_mode validated at config load");
                if let Some(fd) = inherit_unix_fd {
                    info!("Inheriting Unix socket listener from parent process (fd={fd}, path={path})");
                    match adopt_inherited_unix_listener(
                        fd,
                        &path,
                        mode,
                        inherit_unix_socket_ownership,
                    ) {
                        Ok(listener) => {
                            let ownership = match inherit_unix_socket_ownership {
                                Some((dev, ino)) => {
                                    UnixSocketOwnership::capture_expected(&path, dev, ino)
                                }
                                None => UnixSocketOwnership::capture(&path),
                            };
                            match ownership {
                                Ok(ownership) => (Some(listener), Some(ownership)),
                                Err(err) => {
                                    error!(
                                        "Failed to verify inherited Unix socket {path} ownership: {err}"
                                    );
                                    std::process::exit(exitcode::OSERR);
                                }
                            }
                        },
                        Err(err) => {
                            error!("Invalid inherited Unix listener fd={fd}: {err}");
                            std::process::exit(exitcode::CONFIG);
                        }
                    }
                } else {
                    match create_unix_listener(&path, mode) {
                        Ok((listener, ownership)) => {
                            info!("Unix socket listening on {path} (mode={mode:#o})");
                            (Some(listener), Some(ownership))
                        }
                        Err(err) => {
                            error!("{err}");
                            std::process::exit(exitcode::OSERR);
                        }
                    }
                }
            }
            None => {
                if let Some(fd) = inherit_unix_fd {
                    warn!(
                        "Ignoring inherited Unix listener fd={fd}: unix_socket_dir is not configured"
                    );
                    unsafe { libc::close(fd) };
                }
                (None, None)
            }
        };

        config.show();

        // Pin the shard count of the global query interners before any
        // client traffic can reach `intern_query`. The lazy DashMaps pick
        // this up on first deref via `new_dashmap_with_capacity`, matching
        // the rest of the project's k8s-safe sharding policy.
        crate::server::set_interner_worker_threads(config.general.worker_threads);

        // Tracks which client is connected to which server for query cancellation.
        let client_server_map: ClientServerMap =
            Arc::new(crate::utils::dashmap::new_dashmap(config.general.worker_threads));

        // Statistics reporting.
        REPORTER.store(Arc::new(Reporter::default()));

        // Connection pool that allows to query all databases.
        match ConnectionPool::from_config(client_server_map.clone()).await {
            Ok(_) => (),
            Err(err) => {
                error!("Failed to initialize connection pools: {err}");
                std::process::exit(exitcode::CONFIG);
            }
        };

        // NOW pools are loaded AND the main listener is
        // accepting. Flip the readiness gate so k8s /ready transitions
        // from 503 ("starting up") to 200 ("ready"). The doc on `READY`
        // is now satisfied: (a) from_config returned Ok, (b) main PG
        // listener has been bound and is accepting (line 443).
        READY.store(true, Ordering::Release);
        info!("Pools initialized; readiness gate open");

        // Static info gauges (build_info, users_configured, log_level)
        // need a populated config and an initialised log controller, so
        // the first refresh runs after pool init. RELOAD calls the same
        // helper, see config::reload_config.
        crate::web::metrics::refresh_static_info_metrics();

        tokio::task::spawn(async move {
            let mut stats_collector = Collector::default();
            stats_collector.collect().await;
        });

        // Socket-state gauges (and the `[sockets]` line in the periodic
        // stats logger) read from a background-refreshed cache so neither
        // path walks /proc/<pid>/fd in the request thread. Refreshing every
        // 15 s sits comfortably below typical Prometheus scrape intervals.
        #[cfg(target_os = "linux")]
        crate::stats::spawn_socket_states_refresh(Duration::from_secs(15));

        tokio::task::spawn(async move {
            retain::retain_connections().await;
        });

        // Dynamic pool GC — cheap no-op when DYNAMIC_POOLS is empty
        {
            let gc_interval = config.general.retain_connections_time.as_std();
            crate::pool::gc::spawn_dynamic_pool_gc(gc_interval);
        }

        // One-shot lifecycle marker so /api/events has at least one entry
        // immediately after boot — operators opening the UI on a fresh
        // pooler get a "process started" annotation on Overview/Wall
        // without waiting for the first admin command. Force `STARTED_AT`
        // to materialize here so the cached timestamp matches what
        // `/api/overview` and `/api/process` report.
        let _ = *STARTED_AT;
        crate::admin::events::push_event(
            "PROCESS_START",
            format!(
                "pg_doorman {} started, pid={}",
                env!("CARGO_PKG_VERSION"),
                std::process::id()
            ),
        );

        // Query interner GC: bounds NAMED via passive Arc::strong_count and
        // ANON via per-entry TTL. Sweep ticks at gc_interval / 4 so an entry
        // marked on cycle N has roughly a quarter-interval to be touched and
        // unmarked before cycle N+1 evicts it. anon_idle_ttl_seconds = 0 maps
        // to u64::MAX milliseconds — disables TTL eviction entirely.
        // gc_interval_seconds = 0 is rejected by Config::validate, so we can
        // assume a strictly positive interval here.
        //
        // anon_idle_ttl is re-read from the live config every tick, so RELOAD
        // takes effect without a restart. gc_interval_seconds is captured at
        // startup and is restart-only — changing the sweep cadence at runtime
        // would require recreating the ticker, which adds complexity for a
        // knob that operators rarely tune live.
        {
            let gc_interval =
                Duration::from_secs(config.general.query_interner_gc_interval_seconds);
            let sweep_interval = gc_interval / 4;
            assert!(
                !sweep_interval.is_zero(),
                "query_interner_gc_interval_seconds must produce a non-zero sweep interval; \
                 Config::validate should have caught a value of 0"
            );

            let initial_ttl_secs = config.general.query_interner_anon_idle_ttl_seconds;
            tokio::task::spawn(async move {
                let mut ticker = tokio::time::interval(sweep_interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut prev_ttl_secs = initial_ttl_secs;
                loop {
                    ticker.tick().await;

                    let anon_ttl_secs =
                        crate::config::config_arc().general.query_interner_anon_idle_ttl_seconds;
                    if anon_ttl_secs != prev_ttl_secs {
                        // Single line per change so an operator who reloaded
                        // a TTL change has visible evidence the GC task
                        // picked it up — without it the only way to confirm
                        // is to scrape Prometheus and wait for the next
                        // eviction wave.
                        info!(
                            "query interner anon TTL changed: {prev_ttl_secs} -> {anon_ttl_secs} seconds"
                        );
                        prev_ttl_secs = anon_ttl_secs;
                    }
                    let anon_ttl_ms = if anon_ttl_secs == 0 {
                        u64::MAX
                    } else {
                        anon_ttl_secs.saturating_mul(1000)
                    };

                    let started = std::time::Instant::now();
                    let named_stats = gc_sweep_named();
                    let anon_stats = gc_sweep_anon(anon_ttl_ms);
                    let elapsed = started.elapsed().as_secs_f64();

                    record_interner_gc(named_stats, anon_stats, elapsed);

                    // Aggregate sweep summary. Suppressed when nothing was
                    // evicted so a quiet pooler at default INFO sees no churn,
                    // but visible at DEBUG during an incident — together with
                    // the per-entry TRACE lines in `gc_sweep_named` /
                    // `gc_sweep_anon` this is enough to reconstruct what the
                    // interner dropped without scraping Prometheus.
                    if named_stats.evicted > 0 || anon_stats.evicted > 0 {
                        debug!(
                            "query_interner GC: named marked={}, evicted={}, bytes={}; anon marked={}, evicted={}, bytes={}, ttl_ms={}; elapsed={:.3}ms",
                            named_stats.marked,
                            named_stats.evicted,
                            named_stats.bytes,
                            anon_stats.marked,
                            anon_stats.evicted,
                            anon_stats.bytes,
                            anon_ttl_ms,
                            elapsed * 1000.0,
                        );
                    }
                }
            });
        }

        // Web listener (Prometheus exporter + optional UI)
        if config.web.enabled {
            // Build the snapshot through `from_config` so the SSO runtime
            // (and any future config-derived fields) populate on cold
            // start, not only on RELOAD. The function also computes the
            // `ui_active` demote rule; we still log the warning here so
            // operators see it once at startup.
            let opts = WebServerOptions::from_config(&config);
            if config.web.ui && !opts.ui_active {
                log::warn!(
                    "web.ui = true ignored: admin_password is default/empty. \
                     Set a real admin_password to enable the UI; /metrics keeps working."
                );
            }
            let ui_active_for_reaper = opts.ui_active;
            let host = format!("{}:{}", config.web.host, config.web.port);
            // Bind synchronously so a port conflict fails the whole startup
            // instead of leaving the daemon "ready" while /metrics + UI
            // silently die in a panicked detached task.
            let web_listener = match crate::web::bind_web_listener(&host) {
                Ok(l) => l,
                Err(e) => {
                    error!("web listener bind failed on {host}: {e}");
                    std::process::exit(exitcode::OSERR);
                }
            };
            tokio::task::spawn(async move {
                crate::web::serve_on(web_listener, opts).await;
            });
            // LogTap stays off until /api/logs is hit; the reaper turns it
            // back off when nobody is polling, so spawn it once here.
            if ui_active_for_reaper && config.web.log_tap_max_entries > 0 {
                tokio::task::spawn(crate::web::log_tap::run_reaper());
            }
        }

        // Signal readiness to parent process (for binary upgrade in foreground mode).
        // capture libc::write / libc::close return
        // codes. Previously both were called without inspection - if
        // `write` returned -1 (EPIPE if parent died, EBADF if parent
        // already closed) the child still proceeded, but the parent
        // never observed the readiness byte and killed the still-
        // coming-up child after the 10-second poll timeout. Logging
        // the errno here gives the child-side cause that operators
        // can correlate with the parent's "did not signal readiness
        // within 10s" message.
        #[cfg(not(windows))]
        if let Ok(ready_fd_str) = std::env::var("PG_DOORMAN_READY_FD") {
            if let Ok(ready_fd) = ready_fd_str.parse::<i32>() {
                info!("Signaling readiness to parent process (fd={ready_fd})");
                let ready_signal: [u8; 1] = [1];
                unsafe {
                    let written = libc::write(
                        ready_fd,
                        ready_signal.as_ptr() as *const libc::c_void,
                        1,
                    );
                    if written != 1 {
                        let err = std::io::Error::last_os_error();
                        warn!(
                            "[binary-upgrade] readiness write to fd={ready_fd} failed: \
                             returned {written}, errno={err}; parent may time out"
                        );
                    }
                    if libc::close(ready_fd) != 0 {
                        let err = std::io::Error::last_os_error();
                        warn!(
                            "[binary-upgrade] readiness close of fd={ready_fd} failed: {err}"
                        );
                    }
                }
                // Remove the env var so it's not inherited by any future child processes
                std::env::remove_var("PG_DOORMAN_READY_FD");
            }
        }

        // Migration receiver is spawned below after tls_acceptor is available.
        // signal handlers were moved to the top of
        // `runtime.block_on` so they survive a SIGTERM during the
        // slow `from_config` boot path.

        let (exit_tx, mut exit_rx) = mpsc::channel::<()>(1);
        let mut admin_only = false;
        #[cfg(unix)]
        let mut _migration_handles: Option<MigrationHandles> = None;

        // Detect foreground + TTY mode: SIGINT should only do graceful shutdown (no binary upgrade).
        // PG_DOORMAN_CI_SHUTDOWN_ONLY=1 forces shutdown-only mode for testing in non-TTY environments.
        let is_foreground_tty = {
            #[cfg(not(windows))]
            {
                use std::io::IsTerminal;
                let force_shutdown = std::env::var("PG_DOORMAN_CI_SHUTDOWN_ONLY")
                    .map(|v| v == "1")
                    .unwrap_or(false);
                force_shutdown || (!args.daemon && std::io::stdin().is_terminal())
            }
            #[cfg(windows)]
            {
                false
            }
        };

        let tls_rate_limiter = tls_state.rate_limiter.clone();
        let tls_acceptor = tls_state.acceptor.clone();
        let migration_receiver_active = Arc::new(AtomicBool::new(false));
        let migration_fresh_accept_released = Arc::new(AtomicBool::new(true));
        let migration_receiver_drained = Arc::new(Notify::new());

        #[cfg(not(windows))]
        if let Ok(counter_str) = std::env::var("PG_DOORMAN_MIGRATION_COUNTER") {
            match counter_str.parse::<usize>() {
                Ok(high_water) => {
                    TOTAL_CONNECTION_COUNTER.fetch_max(high_water, Ordering::Relaxed);
                    info!(
                        "Migration counter high-water mark inherited from parent: {high_water}"
                    );
                }
                Err(err) => warn!(
                    "Invalid PG_DOORMAN_MIGRATION_COUNTER value {counter_str:?}: {err}"
                ),
            }
            std::env::remove_var("PG_DOORMAN_MIGRATION_COUNTER");
        }

        // Spawn migration receiver if parent passed a migration socket
        #[cfg(not(windows))]
        if let Ok(fd_str) = std::env::var("PG_DOORMAN_MIGRATION_FD") {
            if let Ok(migration_fd) = fd_str.parse::<i32>() {
                info!(
                    "Migration socket received from parent (fd={migration_fd})"
                );
                set_fd_close_on_exec(migration_fd, "migration receiver socket");
                std::env::remove_var("PG_DOORMAN_MIGRATION_FD");
                migration_receiver_active.store(true, Ordering::Release);
                migration_fresh_accept_released.store(false, Ordering::Release);
                let migration_receiver_active = Arc::clone(&migration_receiver_active);
                let migration_fresh_accept_released =
                    Arc::clone(&migration_fresh_accept_released);
                let migration_receiver_drained = Arc::clone(&migration_receiver_drained);
                let migration_client_server_map = client_server_map.clone();
                let migration_tls_acceptor = tls_acceptor.clone();
                tokio::spawn(async move {
                    migration_receiver_task(
                        migration_fd,
                        migration_client_server_map,
                        migration_tls_acceptor,
                    )
                    .await;
                    migration_receiver_active.store(false, Ordering::Release);
                    migration_fresh_accept_released.store(true, Ordering::Release);
                    migration_receiver_drained.notify_waiters();
                });
            }
        }

        // Wrap listener in Option to allow dropping it during foreground binary upgrade
        // while still continuing the graceful shutdown process
        let mut listener = Some(listener);

        info!("Accepting connections");

        // Notify systemd that the service is ready to accept connections.
        // No-op when NOTIFY_SOCKET is not set (non-systemd environments).
        if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Ready]) {
            error!("sd_notify READY failed: {e}. If running under systemd Type=notify, the service will not reach active state.");
        }
        loop {
            // Create upgrade signal future (SIGUSR2 on unix, never resolves on windows)
            let upgrade_future = async {
                #[cfg(not(windows))]
                {
                    upgrade_signal.recv().await;
                }
                #[cfg(windows)]
                {
                    std::future::pending::<()>().await;
                }
            };

            // Create accept future only if listener is available
            let migration_receiver_active_for_accept = Arc::clone(&migration_receiver_active);
            let migration_fresh_accept_released_for_accept =
                Arc::clone(&migration_fresh_accept_released);
            let migration_receiver_drained_for_accept = Arc::clone(&migration_receiver_drained);
            let migration_receiver_active_for_unix = Arc::clone(&migration_receiver_active);
            let migration_fresh_accept_released_for_unix =
                Arc::clone(&migration_fresh_accept_released);
            let migration_receiver_drained_for_unix = Arc::clone(&migration_receiver_drained);
            let accept_future = async {
                wait_for_migration_receiver_drain(
                    &migration_receiver_active_for_accept,
                    &migration_fresh_accept_released_for_accept,
                    &migration_receiver_drained_for_accept,
                )
                .await;
                if let Some(ref l) = listener {
                    l.accept().await
                } else {
                    // Listener was dropped (foreground binary upgrade), wait forever
                    std::future::pending().await
                }
            };

            tokio::select! {

                // Reload config:
                // kill -SIGHUP $(pgrep pg_doorman)
                _ = sighup_signal.recv() => {
                    info!("Reloading config");
                    match reload_config(client_server_map.clone()).await {
                        Ok(true) => {
                            crate::admin::events::push_event("RELOAD", "config reloaded (SIGHUP)".to_string());
                        }
                        Ok(false) => {
                            // No-op reload — file re-parsed identically. Still
                            // emit a RELOAD entry with "config unchanged" so
                            // audit-driven SIGHUP'ing leaves a trace; one
                            // event per signal is the natural rate.
                            crate::admin::events::push_event("RELOAD", "config unchanged (SIGHUP)".to_string());
                        }
                        Err(e) => {
                            error!("Config reload rejected: {e}");
                            crate::admin::events::push_event_rate_limited(
                                "CONFIG_VALIDATION_ERROR",
                                format!("SIGHUP reload rejected: {e}"),
                            );
                        }
                    }
                    get_config().show();
                },

                // SIGINT handler:
                // - Foreground + TTY (Ctrl+C): graceful shutdown only (no binary upgrade)
                // - Daemon / no TTY: legacy binary upgrade + graceful shutdown
                _ = interrupt_signal.recv() => {
                    if is_foreground_tty {
                        // Foreground + TTY: graceful shutdown only (no binary upgrade)
                        info!("Got SIGINT (Ctrl+C), starting graceful shutdown");
                        SHUTDOWN_IN_PROGRESS.store(true, Ordering::SeqCst);
                        retain::drain_all_pools();
                        if admin_only { continue; }
                        admin_only = true;
                        let shutdown_timeout = live_shutdown_timeout();
                        spawn_shutdown_timer(exit_tx.clone(), shutdown_timeout);
                        continue;
                    }

                    // Daemon / no TTY: legacy binary upgrade + graceful shutdown
                    #[cfg(not(windows))]
                    {
                        info!("Got SIGINT, starting binary upgrade and graceful shutdown");
                        let shutdown_timeout = live_shutdown_timeout();
                        match binary_upgrade_and_shutdown(
                            &args,
                            admin_only,
                            &mut listener,
                            &mut unix_listener,
                            &mut unix_socket_ownership,
                            shutdown_timeout,
                            &exit_tx,
                        ).await {
                            None => continue,
                            handles => { _migration_handles = handles; }
                        }
                        admin_only = true;
                    }
                },

                // SIGUSR2: binary upgrade + graceful shutdown (recommended, works in all modes)
                // kill -USR2 $(pgrep pg_doorman)
                _ = upgrade_future => {
                    #[cfg(not(windows))]
                    {
                        info!("Got SIGUSR2, starting binary upgrade and graceful shutdown");
                        let shutdown_timeout = live_shutdown_timeout();
                        match binary_upgrade_and_shutdown(
                            &args,
                            admin_only,
                            &mut listener,
                            &mut unix_listener,
                            &mut unix_socket_ownership,
                            shutdown_timeout,
                            &exit_tx,
                        ).await {
                            None => continue,
                            handles => { _migration_handles = handles; }
                        }
                        admin_only = true;
                    }
                },

                _ = term_signal.recv() => {
                    let clients_in_tx = CLIENTS_IN_TRANSACTIONS.load(Ordering::Relaxed);
                    info!("Got SIGTERM, closing with {clients_in_tx} clients in transactions");
                    // notify systemd that we are
                    // intentionally stopping. Without this, systemd
                    // waits TimeoutStopSec (default 90s) while the
                    // pooler gracefully drains; service status shows
                    // "deactivating" with no hint that the drain is
                    // intentional.
                    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]) {
                        warn!("sd_notify STOPPING failed: {e}");
                    }
                    break;
                },

                // new client.
                new_client = accept_future => {
                    let (mut socket, addr) = match new_client {
                        Ok((socket, addr)) => (socket, addr),
                        Err(err) => {
                            // EMFILE/ENFILE on accept means the process fd
                            // table is full. Without a backoff the loop
                            // re-arms immediately on every queued SYN —
                            // CPU spins, the log gets thousands of
                            // identical lines per millisecond, and nothing
                            // is freed by the loop itself. Sleep so the
                            // kernel can drain its SYN-ack retry budget
                            // (clients eventually give up) and so other
                            // tasks have a chance to release fds. The log
                            // is throttled to one line per 5 s; the
                            // backoff prevents tight-loop CPU burn.
                            if is_fd_exhaustion_io(&err) {
                                if should_log_accept_resource_now() {
                                    error!(
                                        "Failed to accept new connection: {err} \
                                         (process fd table exhausted; backing off)"
                                    );
                                }
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            } else {
                                error!("Failed to accept new connection: {err}");
                            }
                            continue;
                        }
                    };
                    if admin_only {
                        warn!("Rejecting connection from {addr}: pooler shutting down");
                        let _ = socket.shutdown().await;
                        continue;
                    }
                    let tls_rate_limiter = tls_rate_limiter.clone();
                    let tls_acceptor = tls_acceptor.clone();
                    let client_server_map = client_server_map.clone();
                    // Per-connection hot path: borrow the live Arc<Config>
                    // instead of deep-cloning the whole Config just to read
                    // two scalar `general` fields below.
                    let config = config_arc();

                    // copy/paste fix - TCP accept
                    // path was reading `log_client_connections` and
                    // using it as the `log_disconnections` flag in
                    // `log_session_end`. Operator with
                    // `log_client_connections = true,
                    // log_client_disconnections = false` would see
                    // disconnect lines on TCP but not on Unix
                    // sockets (which correctly read the right knob
                    // at line ~1010). Mirror Unix.
                    let log_client_disconnections = config.general.log_client_disconnections;
                    let max_connections = config.general.max_connections;

                    configure_tcp_socket(&socket);
                    tokio::task::spawn(async move {
                        let connection_id = TOTAL_CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed) as u64 + 1;
                        // RAII guard - decrement
                        // on every drop (return, panic, abort).
                        let _client_count_guard = ClientCountGuard::acquire();
                        let current_clients = CURRENT_CLIENT_COUNT.load(Ordering::SeqCst);
                        if current_clients as u64 > max_connections {
                            warn!("[#c{connection_id}] client {addr} rejected: too many clients (current={current_clients}, max={max_connections})");
                            if let Err(err) = crate::client::client_entrypoint_too_many_clients_already(
                                socket, client_server_map).await {
                                error!("[#c{connection_id}] client {addr} disconnected with error: {err}");
                            }
                            // Guard drops on return - decrement runs.
                            return;
                        }
                        let start = Utc::now().naive_utc();
                        let result = crate::client::client_entrypoint(
                            socket,
                            client_server_map,
                            admin_only,
                            tls_acceptor,
                            tls_rate_limiter,
                            connection_id,
                        )
                        .await;
                        log_session_end(
                            result,
                            connection_id,
                            &addr.to_string(),
                            start,
                            log_client_disconnections,
                        );
                        // Guard drops here - decrement runs.
                    });
                }

                // Unix socket client
                new_unix = async {
                    wait_for_migration_receiver_drain(
                        &migration_receiver_active_for_unix,
                        &migration_fresh_accept_released_for_unix,
                        &migration_receiver_drained_for_unix,
                    )
                    .await;
                    if let Some(ref l) = unix_listener {
                        l.accept().await
                    } else {
                        std::future::pending().await
                    }
                } => {
                    let (socket, _unix_addr) = match new_unix {
                        Ok(pair) => pair,
                        Err(err) => {
                            // Same EMFILE/ENFILE backoff as the TCP accept
                            // loop above. Without it an exhausted fd table
                            // turns this branch into a tight loop.
                            if is_fd_exhaustion_io(&err) {
                                if should_log_accept_resource_now() {
                                    error!(
                                        "Failed to accept Unix connection: {err} \
                                         (process fd table exhausted; backing off)"
                                    );
                                }
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            } else {
                                error!("Failed to accept Unix connection: {err}");
                            }
                            continue;
                        }
                    };
                    if admin_only {
                        drop(socket);
                        continue;
                    }
                    configure_unix_socket(&socket);
                    let client_server_map = client_server_map.clone();
                    // Per-connection hot path: borrow the live Arc<Config>
                    // instead of deep-cloning the whole Config just to read
                    // two scalar `general` fields below.
                    let config = config_arc();
                    let log_client_disconnections = config.general.log_client_disconnections;
                    let max_connections = config.general.max_connections;

                    tokio::task::spawn(async move {
                        let connection_id = TOTAL_CONNECTION_COUNTER.fetch_add(1, Ordering::Relaxed) as u64 + 1;
                        // RAII guard (Unix path).
                        let _client_count_guard = ClientCountGuard::acquire();
                        let current_clients = CURRENT_CLIENT_COUNT.load(Ordering::SeqCst);
                        if current_clients as u64 > max_connections {
                            warn!("[#c{connection_id}] unix client rejected: too many clients (current={current_clients}, max={max_connections})");
                            if let Err(err) = crate::client::client_entrypoint_too_many_clients_already_unix(
                                socket,
                                connection_id,
                            )
                            .await
                            {
                                warn!("[#c{connection_id}] unix client rejection response failed: {err}");
                            }
                            return;
                        }
                        let start = Utc::now().naive_utc();
                        let result = crate::client::client_entrypoint_unix(
                            socket,
                            client_server_map,
                            admin_only,
                            connection_id,
                        )
                        .await;
                        log_session_end(
                            result,
                            connection_id,
                            "unix:",
                            start,
                            log_client_disconnections,
                        );
                    });
                }

                _ = exit_rx.recv() => {
                    break;
                }

            }
        }
        // Cleanup Unix socket file only if the inode on disk is still the
        // one this process created. During a SIGUSR2 binary upgrade the
        // successor rebinds the same path before we reach this point, so
        // an unconditional unlink here would wipe out the new listener.
        if let Some(ref ownership) = unix_socket_ownership {
            match ownership.cleanup_if_ours() {
                UnixSocketCleanup::Removed => {}
                UnixSocketCleanup::Missing => {}
                UnixSocketCleanup::Skipped { reason } => {
                    info!(
                        "Leaving Unix socket {} in place: {reason}",
                        ownership.path
                    );
                }
                UnixSocketCleanup::Failed { err } => {
                    warn!("Failed to remove Unix socket {}: {err}", ownership.path);
                }
            }
        }

        info!("Shutting down...");

        // Signal migration_sender_task to stop, then wait for it to
        // flush all pending payloads over the Unix socket. Without
        // this, process::exit would kill the sender before it finishes
        // sending data to the new process, losing migrated clients.
        //
        // capture JoinError properly. Pre-fix
        // `let _ = handles.sender_handle.await` silently swallowed
        // panic / cancel - operators saw "Migration sender finished"
        // even if the task crashed and migrated clients were lost.
        #[cfg(unix)]
        if let Some(handles) = _migration_handles.take() {
            drop(handles.shutdown_tx);
            match tokio::time::timeout(MIGRATION_SENDER_DRAIN_TIMEOUT, handles.sender_handle).await
            {
                Ok(Ok(_)) => info!("Migration sender finished"),
                Ok(Err(e)) if e.is_panic() => error!(
                    "Migration sender PANICKED - migrated clients may be lost: {e}"
                ),
                Ok(Err(e)) => warn!("Migration sender join error: {e:?}"),
                Err(_) => warn!(
                    "Migration sender did not finish within {MIGRATION_SENDER_DRAIN_TIMEOUT:?}; continuing shutdown"
                ),
            }
        }

        // drain in-flight graceful Terminate tasks
        // spawned by `Server::drop`. Without this, RELOAD
        // storm + SIGUSR2 with hundreds of idle backends kills
        // almost all the 2 Terminate futures before they reach
        // PostgreSQL - backends observe RST/FIN instead of the
        // explicit Terminate frame and stay alive as zombies until
        // their own `tcp_keepalives_idle` fires.
        let remaining = crate::server::wait_terminate_tasks_drained(
            std::time::Duration::from_secs(2),
        )
        .await;
        if remaining > 0 {
            warn!(
                "Shutdown: {remaining} graceful Terminate task(s) still in flight \
                 after 2s timeout - backends may observe RST/FIN instead of \
                 Terminate frame"
            );
        } else {
            info!("All graceful Terminate tasks drained");
        }

        // Background tokio tasks (stats, retain, prometheus) run in
        // infinite loops — the runtime drop would hang waiting for
        // worker threads to drain them.
        std::process::exit(0);
    });

    Ok(())
}

/// Migration handles returned by binary_upgrade_and_shutdown.
/// Dropping shutdown_tx signals the sender task to exit.
/// Awaiting sender_handle ensures all payloads are flushed to the socket.
#[cfg(not(windows))]
struct MigrationHandles {
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    sender_handle: tokio::task::JoinHandle<()>,
}

#[cfg(not(windows))]
fn notify_systemd_binary_upgrade_stopping() {
    if let Err(e) = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]) {
        warn!("sd_notify STOPPING (binary_upgrade) failed: {e}");
    }
}

#[cfg(not(windows))]
fn read_daemon_pid_file_from_open_file(file: &mut std::fs::File) -> Option<libc::pid_t> {
    use std::io::{Read, Seek, SeekFrom};

    file.seek(SeekFrom::Start(0)).ok()?;
    let mut raw = String::new();
    file.read_to_string(&mut raw).ok()?;
    let parsed = raw.trim().parse::<i64>().ok()?;
    if parsed <= 1 || parsed > libc::pid_t::MAX as i64 {
        return None;
    }
    Some(parsed as libc::pid_t)
}

#[cfg(not(windows))]
fn open_daemon_pid_file_for_signal(path: impl AsRef<std::path::Path>) -> Option<std::fs::File> {
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::OpenOptionsExt;

    let path = path.as_ref();
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(err) => {
            warn!(
                "[binary-upgrade] refusing daemon pid-file signal cleanup for {}: \
                 failed to open safely: {err}",
                path.display()
            );
            return None;
        }
    };

    let Ok(meta) = file.metadata() else {
        warn!(
            "[binary-upgrade] refusing daemon pid-file signal cleanup for {}: \
             failed to inspect opened file",
            path.display()
        );
        return None;
    };
    let file_type = meta.file_type();
    if !file_type.is_file() {
        warn!(
            "[binary-upgrade] refusing daemon pid-file signal cleanup for unsafe path {}: \
             not a regular file",
            path.display()
        );
        return None;
    }
    let Ok(path_meta) = std::fs::symlink_metadata(path) else {
        return None;
    };
    if path_meta.file_type().is_symlink()
        || path_meta.dev() != meta.dev()
        || path_meta.ino() != meta.ino()
    {
        warn!(
            "[binary-upgrade] refusing daemon pid-file signal cleanup for unsafe path {}: \
             path no longer refers to the validated file",
            path.display()
        );
        return None;
    }
    if meta.nlink() != 1 {
        warn!(
            "[binary-upgrade] refusing daemon pid-file signal cleanup for {}: link count is {}",
            path.display(),
            meta.nlink()
        );
        return None;
    }
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        warn!(
            "[binary-upgrade] refusing daemon pid-file signal cleanup for {}: owner uid {} \
             does not match current euid {euid}",
            path.display(),
            meta.uid()
        );
        return None;
    }
    if meta.mode() & 0o022 != 0 {
        warn!(
            "[binary-upgrade] refusing daemon pid-file signal cleanup for {}: mode {:o} is \
             group/other writable",
            path.display(),
            meta.mode() & 0o7777
        );
        return None;
    }
    Some(file)
}

#[cfg(not(windows))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DaemonProcessIdentitySource {
    #[allow(dead_code)]
    PidFileSnapshot,
    TrustedPipe,
}

#[cfg(not(windows))]
impl DaemonProcessIdentitySource {
    fn permits_detached_daemon_signal(self) -> bool {
        match self {
            Self::PidFileSnapshot => false,
            Self::TrustedPipe => true,
        }
    }
}

#[cfg(not(windows))]
#[derive(Debug)]
struct DaemonProcessIdentity {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pid: libc::pid_t,
    source: DaemonProcessIdentitySource,
    #[cfg(target_os = "linux")]
    start_time_ticks: u64,
    #[cfg(target_os = "linux")]
    pidfd: OwnedFd,
}

#[cfg(not(windows))]
impl DaemonProcessIdentity {
    fn permits_detached_daemon_signal(&self) -> bool {
        self.source.permits_detached_daemon_signal()
    }
}

#[cfg(target_os = "linux")]
fn linux_process_start_time_ticks(pid: libc::pid_t) -> Option<u64> {
    if pid <= 1 {
        return None;
    }
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields_start = stat.rfind(") ")? + 2;
    stat[fields_start..]
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(target_os = "linux")]
fn daemon_process_identity(
    pid: libc::pid_t,
    source: DaemonProcessIdentitySource,
) -> Option<DaemonProcessIdentity> {
    let pidfd = pidfd_open(pid).ok()?;
    let start_time_ticks = linux_process_start_time_ticks(pid)?;
    Some(DaemonProcessIdentity {
        pid,
        source,
        start_time_ticks,
        pidfd,
    })
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn daemon_process_identity(
    pid: libc::pid_t,
    source: DaemonProcessIdentitySource,
) -> Option<DaemonProcessIdentity> {
    if pid <= 1 {
        return None;
    }
    Some(DaemonProcessIdentity { pid, source })
}

#[cfg(target_os = "linux")]
fn daemon_process_identity_still_matches(identity: &DaemonProcessIdentity) -> bool {
    linux_process_start_time_ticks(identity.pid) == Some(identity.start_time_ticks)
}

#[cfg(not(windows))]
fn daemon_pid_is_protected(
    pid: libc::pid_t,
    previous_pid: Option<libc::pid_t>,
    wrapper_pid: libc::pid_t,
) -> bool {
    let current_pid = std::process::id() as libc::pid_t;
    pid == current_pid || pid == wrapper_pid || Some(pid) == previous_pid
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn capture_unready_daemon_identity_from_pid_file(
    pid_file: impl AsRef<std::path::Path>,
    previous_pid: Option<libc::pid_t>,
    wrapper_pid: libc::pid_t,
) -> Option<DaemonProcessIdentity> {
    let mut pid_file = open_daemon_pid_file_for_signal(pid_file.as_ref())?;
    let pid = read_daemon_pid_file_from_open_file(&mut pid_file)?;
    if daemon_pid_is_protected(pid, previous_pid, wrapper_pid) {
        return None;
    }
    daemon_process_identity(pid, DaemonProcessIdentitySource::PidFileSnapshot)
}

#[cfg(not(windows))]
fn read_daemon_identity_pid_from_fd(fd: libc::c_int) -> Option<Option<libc::pid_t>> {
    let mut raw = [0_u8; 64];
    let read = unsafe { libc::read(fd, raw.as_mut_ptr() as *mut libc::c_void, raw.len()) };
    if read < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            return None;
        }
        warn!("[binary-upgrade] failed to read daemon successor identity fd={fd}: {err}");
        return Some(None);
    }
    if read == 0 {
        return Some(None);
    }
    let raw = &raw[..read as usize];
    let Ok(text) = std::str::from_utf8(raw) else {
        warn!("[binary-upgrade] daemon successor identity fd={fd} returned non-UTF8 pid");
        return Some(None);
    };
    let Ok(parsed) = text.trim().parse::<i64>() else {
        warn!(
            "[binary-upgrade] daemon successor identity fd={fd} returned invalid pid {:?}",
            text.trim()
        );
        return Some(None);
    };
    if parsed <= 1 || parsed > libc::pid_t::MAX as i64 {
        warn!("[binary-upgrade] daemon successor identity fd={fd} returned unsafe pid {parsed}");
        return Some(None);
    }
    Some(Some(parsed as libc::pid_t))
}

#[cfg(not(windows))]
struct DaemonSuccessorIdentityCapture {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<Option<DaemonProcessIdentity>>>,
}

#[cfg(not(windows))]
impl DaemonSuccessorIdentityCapture {
    fn start_from_fd(
        identity_fd: libc::c_int,
        previous_pid: Option<libc::pid_t>,
        wrapper_pid: libc::pid_t,
    ) -> Self {
        set_fd_nonblocking(identity_fd, "daemon successor identity read end");
        if let Some(pid_result) = read_daemon_identity_pid_from_fd(identity_fd) {
            unsafe {
                libc::close(identity_fd);
            }
            let identity = pid_result.and_then(|pid| {
                if daemon_pid_is_protected(pid, previous_pid, wrapper_pid) {
                    None
                } else {
                    daemon_process_identity(pid, DaemonProcessIdentitySource::TrustedPipe)
                }
            });
            return Self {
                stop: Arc::new(AtomicBool::new(false)),
                handle: Some(std::thread::spawn(move || identity)),
            };
        }
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::spawn(move || loop {
            match read_daemon_identity_pid_from_fd(identity_fd) {
                Some(Some(pid)) => {
                    unsafe {
                        libc::close(identity_fd);
                    }
                    if daemon_pid_is_protected(pid, previous_pid, wrapper_pid) {
                        return None;
                    }
                    return daemon_process_identity(pid, DaemonProcessIdentitySource::TrustedPipe);
                }
                Some(None) => {
                    unsafe {
                        libc::close(identity_fd);
                    }
                    return None;
                }
                None => {
                    if thread_stop.load(Ordering::Relaxed) {
                        unsafe {
                            libc::close(identity_fd);
                        }
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> Option<DaemonProcessIdentity> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.take()?.join().ok().flatten()
    }
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn terminate_unready_daemon_pid_term_only(pid: libc::pid_t) -> bool {
    if pid <= 1 {
        return false;
    }
    let term_rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    if term_rc != 0 {
        warn!(
            "[binary-upgrade] failed to SIGTERM unready detached daemon successor pid {pid}: {}",
            std::io::Error::last_os_error()
        );
        return false;
    }
    warn!(
        "[binary-upgrade] sent SIGTERM to unready detached daemon successor pid {pid}; \
         skipping SIGKILL because this platform lacks stable pid identity verification"
    );
    true
}

#[cfg(target_os = "linux")]
fn pidfd_open(pid: libc::pid_t) -> std::io::Result<OwnedFd> {
    if pid <= 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to open pidfd for pid <= 1",
        ));
    }
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
}

#[cfg(target_os = "linux")]
fn pidfd_send_signal(pidfd: &OwnedFd, signal: libc::c_int) -> std::io::Result<()> {
    let rc = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn terminate_unready_daemon_pid_with_pidfd(
    pid: libc::pid_t,
    expected_identity: Option<DaemonProcessIdentity>,
) -> bool {
    let Some(expected_identity) = expected_identity else {
        warn!(
            "[binary-upgrade] refusing to signal unready detached daemon successor pid {pid}: \
             no process identity was captured during readiness wait"
        );
        return false;
    };
    if expected_identity.pid != pid {
        warn!(
            "[binary-upgrade] refusing to signal unready detached daemon successor pid {pid}: \
             captured identity belongs to pid {}",
            expected_identity.pid
        );
        return false;
    }
    if !expected_identity.permits_detached_daemon_signal() {
        warn!(
            "[binary-upgrade] refusing to signal unready detached daemon successor pid {pid}: \
             process identity was captured from pid-file content only"
        );
        return false;
    }
    if !daemon_process_identity_still_matches(&expected_identity) {
        warn!(
            "[binary-upgrade] refusing to signal unready detached daemon successor pid {pid}: \
             process identity changed before pidfd signal"
        );
        return false;
    }

    if let Err(err) = pidfd_send_signal(&expected_identity.pidfd, libc::SIGTERM) {
        warn!(
            "[binary-upgrade] failed to SIGTERM unready detached daemon successor pid {pid} \
             through pidfd: {err}"
        );
        return false;
    }

    std::thread::sleep(Duration::from_millis(100));
    if let Err(err) = pidfd_send_signal(&expected_identity.pidfd, libc::SIGKILL) {
        if err.raw_os_error() != Some(libc::ESRCH) {
            warn!(
                "[binary-upgrade] failed to SIGKILL unready detached daemon successor pid {pid} \
                 through pidfd: {err}"
            );
        }
    }
    true
}

#[cfg(not(windows))]
fn terminate_unready_daemon_from_pid_file(
    pid_file: impl AsRef<std::path::Path>,
    previous_pid: Option<libc::pid_t>,
    wrapper_pid: libc::pid_t,
    successor_identity: Option<DaemonProcessIdentity>,
) -> bool {
    let Some(mut pid_file) = open_daemon_pid_file_for_signal(pid_file.as_ref()) else {
        return false;
    };
    let Some(pid) = read_daemon_pid_file_from_open_file(&mut pid_file) else {
        warn!(
            "[binary-upgrade] daemon successor pid file was absent or invalid; \
             no detached successor to terminate"
        );
        return false;
    };

    if daemon_pid_is_protected(pid, previous_pid, wrapper_pid) {
        warn!(
            "[binary-upgrade] not terminating daemon pid {pid}: it is the current, wrapper, \
             or pre-spawn pid"
        );
        return false;
    }

    let Some(successor_identity) = successor_identity else {
        warn!(
            "[binary-upgrade] refusing to signal unready detached daemon successor pid {pid}: \
             no detached-daemon process identity was captured"
        );
        return false;
    };
    if !successor_identity.permits_detached_daemon_signal() {
        warn!(
            "[binary-upgrade] refusing to signal unready detached daemon successor pid {pid}: \
             process identity was captured from pid-file content only"
        );
        return false;
    }

    warn!("[binary-upgrade] terminating unready detached daemon successor pid {pid}");
    #[cfg(target_os = "linux")]
    let terminated = terminate_unready_daemon_pid_with_pidfd(pid, Some(successor_identity));
    #[cfg(all(not(windows), not(target_os = "linux")))]
    let terminated = {
        let _ = successor_identity;
        terminate_unready_daemon_pid_term_only(pid)
    };
    if !terminated {
        return false;
    }
    true
}

/// Perform binary upgrade (spawn new process) and initiate graceful shutdown.
/// Returns None if upgrade was aborted (e.g. config validation failed).
/// Returns Some(MigrationHandles) if upgrade started with client migration.
#[cfg(not(windows))]
async fn binary_upgrade_and_shutdown(
    args: &Args,
    admin_only: bool,
    listener: &mut Option<tokio::net::TcpListener>,
    unix_listener: &mut Option<tokio::net::UnixListener>,
    unix_socket_ownership: &mut Option<UnixSocketOwnership>,
    shutdown_timeout: Duration,
    exit_tx: &mpsc::Sender<()>,
) -> Option<MigrationHandles> {
    // First, validate configuration of the new binary before proceeding with shutdown
    if !admin_only {
        let exe_path = std::env::args()
            .next()
            .unwrap_or_else(|| "pg_doorman".to_string());

        info!(
            "Validating configuration with: {exe_path} -t {}",
            args.config_file
        );

        let config_test_result = process::Command::new(&exe_path)
            .arg("-t")
            .arg(&args.config_file)
            .env("PG_DOORMAN_CLOSE_INHERITED_FDS", "1")
            .stdout(process::Stdio::piped())
            .stderr(process::Stdio::piped())
            .output();

        match config_test_result {
            Ok(output) => {
                if !output.status.success() {
                    error!(
                        "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
                    );
                    error!(
                        "!!!                    CRITICAL ERROR                               !!!"
                    );
                    error!(
                        "!!!         CONFIGURATION VALIDATION FAILED                        !!!"
                    );
                    error!(
                        "!!!         BINARY UPGRADE ABORTED - SHUTDOWN CANCELLED            !!!"
                    );
                    error!(
                        "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
                    );
                    error!("");
                    error!("The new binary failed configuration validation!");
                    error!("Configuration file: {}", args.config_file);
                    error!("Exit code: {:?}", output.status.code());
                    if !output.stderr.is_empty() {
                        error!("Error output: {}", String::from_utf8_lossy(&output.stderr));
                    }
                    if !output.stdout.is_empty() {
                        error!(
                            "Standard output: {}",
                            String::from_utf8_lossy(&output.stdout)
                        );
                    }
                    error!("");
                    error!(
                        "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
                    );
                    error!(
                        "!!!  FIX THE CONFIGURATION BEFORE ATTEMPTING BINARY UPGRADE AGAIN  !!!"
                    );
                    error!(
                        "!!!  THE SERVER WILL CONTINUE RUNNING WITH THE CURRENT BINARY      !!!"
                    );
                    error!(
                        "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
                    );
                    return None;
                }
                info!("Configuration validation successful");
            }
            Err(e) => {
                // Local fd exhaustion means the validator cannot spawn.
                // It is not a config failure; let the child drain clients.
                if is_fd_exhaustion_io(&e) {
                    warn!(
                        "Skipping pre-flight configuration validation: local fd \
                         table exhausted ({e}). Proceeding with binary upgrade so \
                         the child can drain the parent's fds via migration."
                    );
                } else {
                    error!(
                        "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
                    );
                    error!(
                        "!!!                    CRITICAL ERROR                               !!!"
                    );
                    error!(
                        "!!!         FAILED TO VALIDATE CONFIGURATION                       !!!"
                    );
                    error!(
                        "!!!         BINARY UPGRADE ABORTED - SHUTDOWN CANCELLED            !!!"
                    );
                    error!(
                        "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
                    );
                    error!("");
                    error!("Could not execute configuration test: {e}");
                    error!("Binary path: {exe_path}");
                    error!("");
                    error!(
                        "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
                    );
                    error!(
                        "!!!  THE SERVER WILL CONTINUE RUNNING WITH THE CURRENT BINARY      !!!"
                    );
                    error!(
                        "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
                    );
                    return None;
                }
            }
        }
    }

    // Successor spawn/readiness can still rollback below. Keep
    // client-visible shutdown unpublished until the successor is ready and
    // foreground migration has either been installed or ruled out.

    let mut migration_handles: Option<MigrationHandles> = None;

    // During migration, in-transaction clients still need checked-out servers.
    if admin_only {
        retain::drain_all_pools();
    }

    if !admin_only {
        // Binary upgrade: start new process with inherited listener fd
        let full_exe_args: Vec<_> = std::env::args().collect();
        let exe_path = &full_exe_args[0];
        // Filter out any existing inherited-fd arguments and their values.
        let mut exe_args: Vec<String> = Vec::new();
        let mut skip_next = false;
        for arg in full_exe_args.iter().skip(1) {
            if skip_next {
                skip_next = false;
                continue;
            }
            if arg == "--inherit-fd" || arg == "--inherit-unix-fd" {
                skip_next = true;
                continue;
            }
            if arg.starts_with("--inherit-fd=") || arg.starts_with("--inherit-unix-fd=") {
                continue;
            }
            exe_args.push(arg.to_string());
        }
        core_affinity::clear_for_current();

        let listener_fd = listener.as_ref().unwrap().as_raw_fd();
        let unix_listener_fd = unix_listener.as_ref().map(|l| l.as_raw_fd());
        let mut unix_mode_rollback = match prepare_inherited_unix_listener_mode_for_upgrade(
            args,
            unix_listener_fd,
            unix_socket_ownership,
        )
        .await
        {
            Ok(rollback) => rollback,
            Err(err) => {
                error!("[binary-upgrade] {err}; aborting upgrade");
                publish_migration_in_progress(false);
                SHUTDOWN_IN_PROGRESS.store(false, Ordering::SeqCst);
                return None;
            }
        };

        if args.daemon {
            // Daemon mode: start new daemon process.
            let daemon_pid_file = get_config().general.daemon_pid_file.clone();
            let daemon_pid_before_spawn = Some(std::process::id() as libc::pid_t);
            let daemon_pid_file_fd = daemon::lib::current_pid_file_fd();
            let mut pipe_fds: [libc::c_int; 2] = [0; 2];
            if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
                error!("Failed to create daemon readiness pipe for binary upgrade");
                publish_migration_in_progress(false);
                SHUTDOWN_IN_PROGRESS.store(false, Ordering::SeqCst);
                return None;
            }
            let pipe_read_fd = pipe_fds[0];
            let pipe_write_fd = pipe_fds[1];
            set_fd_close_on_exec(pipe_read_fd, "daemon readiness pipe read end");
            let mut daemon_identity_fds: [libc::c_int; 2] = [0; 2];
            if unsafe { libc::pipe(daemon_identity_fds.as_mut_ptr()) } != 0 {
                error!("Failed to create daemon identity pipe for binary upgrade");
                unsafe {
                    libc::close(pipe_read_fd);
                    libc::close(pipe_write_fd);
                }
                publish_migration_in_progress(false);
                SHUTDOWN_IN_PROGRESS.store(false, Ordering::SeqCst);
                return None;
            }
            let daemon_identity_read_fd = daemon_identity_fds[0];
            let daemon_identity_write_fd = daemon_identity_fds[1];
            set_fd_close_on_exec(
                daemon_identity_read_fd,
                "daemon successor identity pipe read end",
            );

            let spawn_res = unsafe {
                let mut cmd = process::Command::new(exe_path);
                cmd.args(&exe_args)
                    .arg("--inherit-fd")
                    .arg(listener_fd.to_string())
                    .stderr(process::Stdio::null())
                    .stdout(process::Stdio::null())
                    .env("PG_DOORMAN_READY_FD", pipe_write_fd.to_string())
                    .env(DAEMON_IDENTITY_FD_ENV, daemon_identity_write_fd.to_string())
                    .current_dir(std::env::current_dir().unwrap_or_else(|e| {
                        error!("[binary-upgrade] failed to read cwd: {e}; using /");
                        std::path::PathBuf::from("/")
                    }));
                if let Some(daemon_pid_file_fd) = daemon_pid_file_fd {
                    cmd.env(DAEMON_PID_FILE_FD_ENV, daemon_pid_file_fd.to_string());
                }
                if let Some(unix_listener_fd) = unix_listener_fd {
                    cmd.arg("--inherit-unix-fd")
                        .arg(unix_listener_fd.to_string());
                }
                if let Some(ownership) = unix_socket_ownership.as_ref() {
                    cmd.env(INHERITED_UNIX_SOCKET_DEV_ENV, ownership.dev.to_string())
                        .env(INHERITED_UNIX_SOCKET_INO_ENV, ownership.ino.to_string());
                }
                cmd.process_group(0);
                cmd.pre_exec(move || {
                    libc::fcntl(listener_fd, libc::F_SETFD, 0);
                    libc::fcntl(pipe_write_fd, libc::F_SETFD, 0);
                    libc::fcntl(daemon_identity_write_fd, libc::F_SETFD, 0);
                    if let Some(daemon_pid_file_fd) = daemon_pid_file_fd {
                        libc::fcntl(daemon_pid_file_fd, libc::F_SETFD, 0);
                    }
                    if let Some(unix_listener_fd) = unix_listener_fd {
                        libc::fcntl(unix_listener_fd, libc::F_SETFD, 0);
                    }
                    Ok(())
                });
                cmd.spawn()
            };
            let mut child = match spawn_res {
                Ok(c) => c,
                Err(e) => {
                    unsafe {
                        libc::close(pipe_read_fd);
                        libc::close(pipe_write_fd);
                        libc::close(daemon_identity_read_fd);
                        libc::close(daemon_identity_write_fd);
                    }
                    error!("[binary-upgrade] daemon child spawn failed: {e}; aborting upgrade");
                    publish_migration_in_progress(false);
                    SHUTDOWN_IN_PROGRESS.store(false, Ordering::SeqCst);
                    return migration_handles;
                }
            };
            unsafe {
                libc::close(pipe_write_fd);
                libc::close(daemon_identity_write_fd);
            }

            let mut buf: [u8; 1] = [0];
            let wrapper_pid = child.id() as libc::pid_t;
            let successor_identity_capture = DaemonSuccessorIdentityCapture::start_from_fd(
                daemon_identity_read_fd,
                daemon_pid_before_spawn,
                wrapper_pid,
            );
            let ready = wait_for_pipe_readiness(pipe_read_fd, 10_000);
            let successor_identity = successor_identity_capture.finish();
            if ready {
                unsafe {
                    libc::read(pipe_read_fd, buf.as_mut_ptr() as *mut libc::c_void, 1);
                    libc::close(pipe_read_fd);
                }
                info!("New daemon process signaled readiness");
                SHUTDOWN_IN_PROGRESS.store(true, Ordering::SeqCst);
            } else {
                warn!("New daemon process did not signal readiness within 10s");
                unsafe { libc::close(pipe_read_fd) };
                match child.try_wait() {
                    Ok(Some(status)) => {
                        warn!("Daemon upgrade wrapper exited before readiness: {status}");
                    }
                    Ok(None) => {
                        if let Err(e) = child.kill() {
                            warn!("Failed to kill unready daemon upgrade wrapper: {e}");
                        } else {
                            let _ = child.wait();
                        }
                    }
                    Err(e) => {
                        warn!("Failed to inspect unready daemon upgrade wrapper: {e}");
                    }
                }
                terminate_unready_daemon_from_pid_file(
                    &daemon_pid_file,
                    daemon_pid_before_spawn,
                    wrapper_pid,
                    successor_identity,
                );
                if let Err(err) = daemon::lib::rewrite_current_pid_file() {
                    warn!(
                        "[binary-upgrade] failed to restore old daemon pid file after aborted upgrade: {err}"
                    );
                }
                publish_migration_in_progress(false);
                SHUTDOWN_IN_PROGRESS.store(false, Ordering::SeqCst);
                return None;
            }

            if let Err(e) = child.wait() {
                error!(
                    "[binary-upgrade] daemon child wait failed: {e}; \
                     continuing - child may already be detached"
                );
            }
            notify_systemd_binary_upgrade_stopping();
            drop_listener_owner(listener);
            if unix_listener_fd.is_some() {
                if let Some(rollback) = unix_mode_rollback.as_mut() {
                    rollback.disarm();
                }
                drop_unix_listener_owner(unix_listener);
                let _ = unix_socket_ownership.take();
            }
        } else {
            // Foreground mode: start new process with inherited listener fd
            info!("Starting new process with inherited listener fd={listener_fd}");

            // Get current process group to pass to child
            let current_pgid = unsafe { libc::getpgrp() };
            // Create a pipe for readiness signaling
            let mut pipe_fds: [libc::c_int; 2] = [0; 2];
            if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
                error!("Failed to create pipe for binary upgrade");
                publish_migration_in_progress(false);
                SHUTDOWN_IN_PROGRESS.store(false, Ordering::SeqCst);
                return None;
            } else {
                let pipe_read_fd = pipe_fds[0];
                let pipe_write_fd = pipe_fds[1];
                set_fd_close_on_exec(pipe_read_fd, "readiness pipe read end");

                // Create a Unix socketpair for client migration
                let mut migration_fds: [libc::c_int; 2] = [0; 2];
                let migration_ok = unsafe {
                    libc::socketpair(
                        libc::AF_UNIX,
                        libc::SOCK_STREAM,
                        0,
                        migration_fds.as_mut_ptr(),
                    )
                } == 0;
                if !migration_ok {
                    warn!("Failed to create migration socketpair, clients will not be migrated");
                }
                let migration_parent_fd = migration_fds[0]; // kept by old process
                let migration_child_fd = migration_fds[1]; // passed to new process
                if migration_ok {
                    set_fd_close_on_exec(migration_parent_fd, "migration parent socket");
                }

                // Spawn child process with inherited listener fd, pipe, and migration socket
                let parent_connection_counter = TOTAL_CONNECTION_COUNTER.load(Ordering::Relaxed);
                let child_result = unsafe {
                    let mut cmd = process::Command::new(exe_path);
                    cmd.args(&exe_args)
                        .arg("--inherit-fd")
                        .arg(listener_fd.to_string())
                        .env("PG_DOORMAN_READY_FD", pipe_write_fd.to_string())
                        .env(
                            "PG_DOORMAN_MIGRATION_COUNTER",
                            parent_connection_counter.to_string(),
                        );
                    if let Some(unix_listener_fd) = unix_listener_fd {
                        cmd.arg("--inherit-unix-fd")
                            .arg(unix_listener_fd.to_string());
                    }
                    if let Some(ownership) = unix_socket_ownership.as_ref() {
                        cmd.env(INHERITED_UNIX_SOCKET_DEV_ENV, ownership.dev.to_string())
                            .env(INHERITED_UNIX_SOCKET_INO_ENV, ownership.ino.to_string());
                    }
                    if migration_ok {
                        cmd.env("PG_DOORMAN_MIGRATION_FD", migration_child_fd.to_string());
                    }
                    cmd.current_dir(std::env::current_dir().unwrap_or_else(|e| {
                        warn!(
                            "[binary-upgrade] current_dir failed for foreground child: {e}; \
                             using / as fallback"
                        );
                        std::path::PathBuf::from("/")
                    }))
                    .pre_exec(move || {
                        libc::fcntl(listener_fd, libc::F_SETFD, 0);
                        libc::fcntl(pipe_write_fd, libc::F_SETFD, 0);
                        if let Some(unix_listener_fd) = unix_listener_fd {
                            libc::fcntl(unix_listener_fd, libc::F_SETFD, 0);
                        }
                        if migration_ok {
                            libc::fcntl(migration_child_fd, libc::F_SETFD, 0);
                        }
                        libc::setpgid(0, current_pgid);
                        Ok(())
                    });
                    cmd.spawn()
                };

                match child_result {
                    Ok(mut child) => {
                        let child_pid = child.id();
                        unsafe {
                            libc::close(pipe_write_fd);
                            if migration_ok {
                                libc::close(migration_child_fd);
                            }
                        }

                        let mut buf: [u8; 1] = [0];
                        let ready = wait_for_pipe_readiness(pipe_read_fd, 10_000);

                        if ready {
                            unsafe {
                                libc::read(pipe_read_fd, buf.as_mut_ptr() as *mut libc::c_void, 1);
                            }
                            info!("New process signaled readiness");

                            // Hand systemd tracking over to the ready child.
                            if let Err(e) = sd_notify::notify(
                                false,
                                &[sd_notify::NotifyState::MainPid(child_pid)],
                            ) {
                                warn!("sd_notify MAINPID failed: {e}. systemd may restart the service after old process exits.");
                            }
                            notify_systemd_binary_upgrade_stopping();
                        } else {
                            // Timeout or EOF without a readiness byte: keep
                            // the current parent as listener owner.
                            warn!("New process did not signal readiness within 10s (timeout or early exit)");
                            unsafe {
                                libc::close(pipe_read_fd);
                                if migration_ok {
                                    libc::close(migration_parent_fd);
                                }
                            }
                            match child.try_wait() {
                                Ok(Some(status)) => {
                                    warn!("New process exited before readiness: {status}");
                                }
                                Ok(None) => {
                                    if let Err(e) = child.kill() {
                                        warn!(
                                            "Failed to kill unready child process {child_pid}: {e}"
                                        );
                                    } else {
                                        let _ = child.wait();
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        "Failed to inspect unready child process {child_pid}: {e}"
                                    );
                                }
                            }
                            publish_migration_in_progress(false);
                            SHUTDOWN_IN_PROGRESS.store(false, Ordering::SeqCst);
                            return None;
                        }

                        unsafe {
                            libc::close(pipe_read_fd);
                        }
                        drop_listener_owner(listener);
                        if unix_listener_fd.is_some() {
                            if let Some(rollback) = unix_mode_rollback.as_mut() {
                                rollback.disarm();
                            }
                            drop_unix_listener_owner(unix_listener);
                            let _ = unix_socket_ownership.take();
                        }

                        // Queue migration only while live fd headroom can
                        // absorb the dup'd client sockets.
                        let mut notify_migration_waiters = false;
                        if migration_ok {
                            match safe_migration_capacity() {
                                Some(capacity) => {
                                    info!(
                                        "Migration channel capacity: {capacity} \
                                         (max {MIGRATION_CHANNEL_CAPACITY_MAX}; \
                                         bounded by RLIMIT_NOFILE headroom and \
                                         queued migration payload heap budget)"
                                    );
                                    let (tx, rx) = mpsc::channel(capacity);
                                    match MIGRATION_TX.set(tx) {
                                        Ok(()) => {
                                            // Publish the absolute migration deadline BEFORE
                                            // migration_in_progress so any client that observes
                                            // migration_in_progress() also sees the deadline.
                                            let _ = MIGRATION_DEADLINE.set(
                                                tokio::time::Instant::now() + shutdown_timeout,
                                            );
                                            publish_migration_in_progress(true);
                                            let (shutdown_tx, shutdown_rx) =
                                                tokio::sync::oneshot::channel();
                                            let sender_handle =
                                                tokio::spawn(migration_sender_task(
                                                    migration_parent_fd,
                                                    rx,
                                                    shutdown_rx,
                                                ));
                                            migration_handles = Some(MigrationHandles {
                                                shutdown_tx,
                                                sender_handle,
                                            });
                                            info!("Client migration enabled");
                                            notify_migration_waiters = true;
                                        }
                                        Err(_tx) => {
                                            warn!(
                                                "Migration channel already initialized; clients \
                                                 will reconnect to the new process instead of \
                                                 migrating sessions"
                                            );
                                            unsafe { libc::close(migration_parent_fd) };
                                        }
                                    }
                                }
                                None => {
                                    warn!(
                                        "Migration channel disabled: no fd headroom \
                                         left under the current RLIMIT_NOFILE; clients \
                                         will reconnect to the new process instead of \
                                         migrating sessions"
                                    );
                                    // Close the unused parent half while
                                    // graceful shutdown continues.
                                    unsafe { libc::close(migration_parent_fd) };
                                }
                            }
                        }

                        SHUTDOWN_IN_PROGRESS.store(true, Ordering::SeqCst);
                        if notify_migration_waiters {
                            MIGRATION_NOTIFY.notify_waiters();
                        }

                        info!("Foreground binary upgrade complete, listener released");
                    }
                    Err(e) => {
                        error!("Failed to spawn new process: {e}");
                        publish_migration_in_progress(false);
                        SHUTDOWN_IN_PROGRESS.store(false, Ordering::SeqCst);
                        unsafe {
                            libc::close(pipe_read_fd);
                            libc::close(pipe_write_fd);
                            if migration_ok {
                                libc::close(migration_parent_fd);
                                libc::close(migration_child_fd);
                            }
                        }
                        // when child
                        // spawn fails on the foreground binary-upgrade
                        // path, the parent must NOT proceed to
                        // `spawn_shutdown_timer` below. Without this
                        // return, the parent would self-terminate
                        // ~`shutdown_timeout` seconds later even though
                        // the upgrade was aborted and the parent is
                        // healthy. The sibling daemon-mode spawn-Err
                        // arm already returns at line ~1358.
                        return migration_handles;
                    }
                }
            }
        }
    }

    // Don't want this to happen more than once
    if admin_only {
        return migration_handles;
    }

    spawn_shutdown_timer(exit_tx.clone(), shutdown_timeout);
    migration_handles
}

/// Wait for the child readiness byte using `poll(2)`.
///
/// `poll` handles fds above `FD_SETSIZE`; requiring `POLLIN` rejects
/// EOF-only readiness from a child that exited before writing.
///
/// retry on EINTR. A signal landing during the 10 s wait
/// (SIGCHLD from another reaper, an unrelated SIGUSR2) used to make
/// `poll` return `-1` with `errno == EINTR` - indistinguishable from
/// a real timeout - and the parent killed a still-coming-up child.
#[cfg(not(windows))]
fn wait_for_pipe_readiness(pipe_read_fd: libc::c_int, timeout_ms: libc::c_int) -> bool {
    use std::time::Instant;
    let deadline = if timeout_ms < 0 {
        None
    } else {
        Some(Instant::now() + Duration::from_millis(timeout_ms as u64))
    };
    loop {
        let mut pfd = libc::pollfd {
            fd: pipe_read_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let remaining_ms = match deadline {
            Some(d) => {
                let now = Instant::now();
                if now >= d {
                    0
                } else {
                    (d - now).as_millis().min(i32::MAX as u128) as libc::c_int
                }
            }
            None => -1,
        };
        let result = unsafe { libc::poll(&mut pfd, 1, remaining_ms) };
        if result < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                // Spurious wakeup - recompute remaining budget and retry.
                continue;
            }
            return false;
        }
        return result > 0 && (pfd.revents & libc::POLLIN) != 0;
    }
}

/// Spawn a task that waits for all clients to disconnect (or timeout) and then signals exit.
fn spawn_shutdown_timer(exit_tx: mpsc::Sender<()>, shutdown_timeout: Duration) {
    tokio::task::spawn(async move {
        let clients_in_tx = CLIENTS_IN_TRANSACTIONS.load(Ordering::Relaxed);
        let clients_total = CURRENT_CLIENT_COUNT.load(Ordering::Relaxed);
        info!(
            "waiting for {} client{} to disconnect ({} in transactions)",
            clients_total,
            if clients_total == 1 { "" } else { "s" },
            clients_in_tx
        );

        // Poll frequently to detect client count reaching zero quickly,
        // but enforce the overall shutdown_timeout deadline.
        // Drain idle server connections once per second (not every poll tick)
        // to avoid interfering with binary upgrade readiness.
        let poll_interval = Duration::from_millis(250);
        let mut interval = tokio::time::interval(poll_interval);
        // Skip - shutdown poll under a runtime stall should
        // not fire backlogged drain ticks all at once and starve the
        // binary-upgrade ready-check at the same time.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let start = std::time::Instant::now();
        let mut last_drain = std::time::Instant::now();

        loop {
            interval.tick().await;

            // Only drain pools when NOT migrating. During migration,
            // in-transaction clients need their server connections.
            if !migration_in_progress() && last_drain.elapsed() >= Duration::from_secs(1) {
                retain::drain_all_pools();
                last_drain = std::time::Instant::now();
            }

            let clients_in_tx = CLIENTS_IN_TRANSACTIONS.load(Ordering::Relaxed);
            let clients_total = CURRENT_CLIENT_COUNT.load(Ordering::Relaxed);
            if clients_total == 0 {
                info!("All clients disconnected, shutting down");
                let _ = exit_tx.send(()).await;
                return;
            }

            if start.elapsed() >= shutdown_timeout {
                error!(
                    "Graceful shutdown timed out. {} client{} remain ({} in transactions), closing forcibly",
                    clients_total,
                    if clients_total == 1 { "" } else { "s" },
                    clients_in_tx
                );
                let _ = exit_tx.send(()).await;
                return;
            }
        }
    });
}

/// Identity of a Unix socket file this process bound to, captured as
/// `(dev, ino)` plus the original path. Used to decide at shutdown whether
/// the inode on disk is still ours or has been replaced by a successor
/// process during a binary upgrade.
#[cfg(unix)]
#[derive(Debug, Clone)]
struct UnixSocketOwnership {
    path: String,
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
struct UnixSocketModeRollback {
    fd: libc::c_int,
    path: String,
    original_mode: u32,
    active: bool,
}

#[cfg(unix)]
impl UnixSocketModeRollback {
    fn tighten_if_needed(
        fd: libc::c_int,
        path: &str,
        target_mode: u32,
    ) -> std::io::Result<Option<Self>> {
        let original_mode = unix_fd_mode(fd)?;
        if original_mode & !target_mode == 0 {
            return Ok(None);
        }

        set_unix_fd_mode(fd, target_mode)?;
        Ok(Some(Self {
            fd,
            path: path.to_string(),
            original_mode,
            active: true,
        }))
    }

    fn disarm(&mut self) {
        self.active = false;
    }
}

#[cfg(unix)]
impl Drop for UnixSocketModeRollback {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Err(err) = set_unix_fd_mode(self.fd, self.original_mode) {
            warn!(
                "Failed to restore Unix socket {} fd={} mode to {:#o} after aborted upgrade: {err}",
                self.path, self.fd, self.original_mode
            );
        }
    }
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum UnixSocketCleanup {
    /// The inode matched; the file has been removed.
    Removed,
    /// Nothing was on disk at the captured path.
    Missing,
    /// A different inode sits at the path — a successor rebound it.
    Skipped { reason: String },
    /// Removal was attempted but the syscall returned an error.
    Failed { err: String },
}

#[cfg(unix)]
impl UnixSocketOwnership {
    /// Stat the path and remember `(dev, ino)` so future cleanup can verify
    /// the inode has not been replaced.
    fn capture(path: &str) -> Result<Self, std::io::Error> {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::metadata(path)?;
        Ok(Self {
            path: path.to_string(),
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }

    /// Capture inherited ownership after verifying the configured path still
    /// names the inode captured by the parent before spawning this process.
    fn capture_expected(path: &str, dev: u64, ino: u64) -> Result<Self, std::io::Error> {
        ensure_unix_path_matches_ownership(path, dev, ino)?;
        Ok(Self {
            path: path.to_string(),
            dev,
            ino,
        })
    }

    /// Remove the socket file only if the inode on disk still matches the
    /// one captured at `capture` time.
    fn cleanup_if_ours(&self) -> UnixSocketCleanup {
        match Self::inspect(&self.path, self.dev, self.ino) {
            CleanupDecision::Remove => match std::fs::remove_file(&self.path) {
                Ok(()) => UnixSocketCleanup::Removed,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    UnixSocketCleanup::Missing
                }
                Err(err) => UnixSocketCleanup::Failed {
                    err: err.to_string(),
                },
            },
            CleanupDecision::Missing => UnixSocketCleanup::Missing,
            CleanupDecision::Skip(reason) => UnixSocketCleanup::Skipped { reason },
        }
    }

    /// Pure decision function: given a path and the expected `(dev, ino)`,
    /// should the caller proceed to unlink the file? Split out so the logic
    /// can be unit-tested without touching real filesystem state.
    fn inspect(path: &str, expected_dev: u64, expected_ino: u64) -> CleanupDecision {
        use std::os::unix::fs::MetadataExt;
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                let dev = meta.dev();
                let ino = meta.ino();
                if dev == expected_dev && ino == expected_ino {
                    CleanupDecision::Remove
                } else {
                    CleanupDecision::Skip(format!(
                        "inode changed (expected dev={expected_dev} ino={expected_ino}, found dev={dev} ino={ino}); another process owns the path now"
                    ))
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => CleanupDecision::Missing,
            Err(err) => CleanupDecision::Skip(format!("stat failed: {err}")),
        }
    }
}

#[cfg(unix)]
async fn prepare_inherited_unix_listener_mode_for_upgrade(
    args: &Args,
    unix_listener_fd: Option<libc::c_int>,
    unix_socket_ownership: &Option<UnixSocketOwnership>,
) -> Result<Option<UnixSocketModeRollback>, String> {
    let Some(fd) = unix_listener_fd else {
        return Ok(None);
    };
    let Some(ownership) = unix_socket_ownership.as_ref() else {
        return Err("Unix listener fd exists without socket ownership metadata".to_string());
    };

    match UnixSocketOwnership::inspect(&ownership.path, ownership.dev, ownership.ino) {
        CleanupDecision::Remove => {}
        CleanupDecision::Missing => {
            return Err(format!(
                "Unix socket {} disappeared before binary upgrade spawn",
                ownership.path
            ));
        }
        CleanupDecision::Skip(reason) => {
            return Err(format!(
                "Unix socket {} no longer belongs to this process before binary upgrade: {reason}",
                ownership.path
            ));
        }
    }

    let target_config = crate::config::parse_unpublished_config(&args.config_file)
        .await
        .map_err(|err| err.to_string())?;
    let Some(dir) = target_config.general.unix_socket_dir.as_ref() else {
        return Ok(None);
    };
    let target_path = format!("{dir}/.s.PGSQL.{}", target_config.general.port);
    if target_path != ownership.path {
        return Ok(None);
    }
    let target_mode =
        crate::config::General::parse_unix_socket_mode(&target_config.general.unix_socket_mode)
            .map_err(|err| err.to_string())?;

    UnixSocketModeRollback::tighten_if_needed(fd, &ownership.path, target_mode).map_err(|err| {
        format!(
            "Failed to apply pre-upgrade Unix socket mode via inherited fd; refusing path chmod: {err}"
        )
    })
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum CleanupDecision {
    Remove,
    Missing,
    Skip(String),
}

/// Log the end of a client session using a shared format string. Both the
/// TCP and Unix accept branches used to inline the same match on
/// `Result<Option<ClientSessionInfo>, Error>` — same identity string,
/// same elapsed-time rendering, same warn vs info split. Centralising
/// it keeps the two remaining accept sites down to a single call.
fn log_session_end(
    result: Result<Option<crate::client::ClientSessionInfo>, crate::errors::Error>,
    connection_id: u64,
    peer_label: &str,
    session_start: chrono::NaiveDateTime,
    log_disconnections: bool,
) {
    let session = format_duration(&(Utc::now().naive_utc() - session_start));
    match result {
        Ok(session_info) => {
            if log_disconnections || log::log_enabled!(log::Level::Debug) {
                let identity = match &session_info {
                    Some(si) => {
                        format!("[{}@{} #c{}]", si.username, si.pool_name, si.connection_id)
                    }
                    None => format!("[#c{connection_id}]"),
                };
                info!("{identity} client disconnected from {peer_label}, session={session}");
            }
        }
        Err(err) => {
            // Pre-auth failures: identity unknown, only connection_id available.
            // Post-auth failures already logged with [user@pool #cN] inside entrypoint.
            warn!("[#c{connection_id}] client {peer_label} disconnected with error: {err}, session={session}");
        }
    }
}

/// Create a Tokio Unix socket listener at `path` with the given permission
/// `mode`.
///
/// This is the whole bring-up sequence the pooler runs at startup, factored
/// out of `run_server` so unit tests can reproduce the failure modes (stale
/// file, dead-end directory, chmod failure) in a tempdir without launching a
/// full server. On success the returned [`UnixSocketOwnership`] records the
/// (dev, ino) of the inode so the shutdown path can decide whether the
/// successor of a binary upgrade has already replaced it.
#[cfg(unix)]
fn create_unix_listener(
    path: &str,
    mode: u32,
) -> Result<(tokio::net::UnixListener, UnixSocketOwnership), String> {
    prepare_unix_socket_path(path)
        .map_err(|err| format!("Cannot reuse Unix socket path {path}: {err}"))?;

    // Clamp the umask so the socket inode created by bind() never exists with
    // weaker permissions than `mode`. Without this a concurrent client
    // connecting in the window between bind() and set_permissions() would
    // land on the umask-derived rights (typically 0644) and bypass the
    // configured restriction. set_permissions() still runs afterwards so
    // callers can loosen the mode (e.g. 0660 with a group bit).
    let restrict_bits = !(mode & 0o777) & 0o777;
    let _umask_guard = UmaskGuard::restrict(restrict_bits as libc::mode_t);

    let listener = tokio::net::UnixListener::bind(path)
        .map_err(|err| format!("Failed to bind Unix socket {path}: {err}"))?;
    drop(_umask_guard);

    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|err| format!("Failed to set mode {mode:#o} on Unix socket {path}: {err}"))?;

    let ownership = UnixSocketOwnership::capture(path)
        .map_err(|err| format!("Failed to stat Unix socket {path} after bind: {err}"))?;

    Ok((listener, ownership))
}

/// Prepare a Unix socket path for bind() by clearing any stale file without
/// clobbering a live peer.
///
/// Shared directories like `/var/run/postgresql` may contain another process's
/// live socket. This helper:
///
/// 1. Returns Ok if nothing exists at the path.
/// 2. Attempts a connect — if it succeeds, a live peer owns the socket and
///    we refuse to touch it so the caller can fail loudly.
/// 3. Otherwise removes the stale inode (typical case after a crash).
///
/// Errors are returned as strings with enough context for the caller to log
/// and exit; unit tests exercise the three branches without touching the
/// process umask or the real server bring-up.
#[cfg(unix)]
fn prepare_unix_socket_path(path: &str) -> Result<(), String> {
    use std::os::unix::net::UnixStream;

    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(format!("stat failed: {err}")),
    }

    // Short probe: a local Unix connect that would succeed does so in
    // microseconds. If it refuses, the socket is stale (no listener bound to
    // the inode) and we can reclaim it.
    match UnixStream::connect(path) {
        Ok(_) => Err(format!(
            "another process is already listening on {path}; refusing to remove it"
        )),
        Err(_) => std::fs::remove_file(path)
            .map_err(|err| format!("failed to remove stale socket {path}: {err}")),
    }
}

/// Temporarily tighten the process umask for the lifetime of the guard.
///
/// The Unix listener startup needs the socket inode to be created with no
/// weaker permissions than the configured `unix_socket_mode`. Since `bind()`
/// applies `0666 & ~umask` at the moment the file appears in the filesystem,
/// we ratchet the umask up, perform the bind, then restore the original
/// value on drop. The guard is also safe to drop explicitly once the socket
/// is in place and `set_permissions` has run.
#[cfg(unix)]
struct UmaskGuard {
    previous: libc::mode_t,
}

#[cfg(unix)]
impl UmaskGuard {
    /// Ensure the process umask masks at least `additional_bits` on top of
    /// whatever was already set.
    fn restrict(additional_bits: libc::mode_t) -> Self {
        // SAFETY: umask is a process-global knob; we snapshot the current
        // value by setting a known mask, OR in our extra bits, and restore
        // it on drop. No Rust invariants are touched.
        let previous = unsafe { libc::umask(0o777) };
        unsafe { libc::umask(previous | additional_bits) };
        Self { previous }
    }
}

#[cfg(unix)]
impl Drop for UmaskGuard {
    fn drop(&mut self) {
        // SAFETY: same rationale as `restrict`; we only touch the umask.
        unsafe { libc::umask(self.previous) };
    }
}

#[cfg(test)]
mod create_unix_listener_tests {
    use super::create_unix_listener;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    // Serialised because the umask_guard_tests in this crate flip the
    // process umask to 0o777 while running; any concurrent tempdir-backed
    // bind() would land on an inaccessible file and fail with EACCES.
    #[tokio::test]
    #[serial]
    async fn binds_and_applies_mode() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(".s.PGSQL.6432");
        let path_str = path.to_str().unwrap();

        let (listener, ownership) =
            create_unix_listener(path_str, 0o600).expect("bind must succeed in empty tempdir");

        let meta = std::fs::metadata(path_str).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(ownership.path, path_str);

        drop(listener);
    }

    #[tokio::test]
    #[serial]
    async fn bind_fails_when_directory_missing() {
        // Directory we never created → bind must return a structured error
        // instead of panicking or exiting the process.
        let dir = tempdir().unwrap();
        let path = dir
            .path()
            .join("does")
            .join("not")
            .join("exist")
            .join(".s.PGSQL.6432");

        let err = create_unix_listener(path.to_str().unwrap(), 0o600)
            .expect_err("bind must fail when parent directory is missing");
        assert!(err.contains("Failed to bind"), "unexpected error: {err}");
    }

    #[tokio::test]
    #[serial]
    async fn group_readable_mode_is_applied() {
        // 0660 exercises the path where set_permissions *loosens* the bits
        // the umask guard masked off; if we mess that up the file stays
        // owner-only and client groups lose access silently.
        let dir = tempdir().unwrap();
        let path = dir.path().join(".s.PGSQL.6432");

        let (listener, _ownership) =
            create_unix_listener(path.to_str().unwrap(), 0o660).expect("bind must succeed");

        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o660);
        drop(listener);
    }
}

#[cfg(test)]
mod unix_socket_ownership_tests {
    use super::{CleanupDecision, UnixSocketCleanup, UnixSocketOwnership};
    use serial_test::serial;
    use std::os::unix::net::UnixListener;
    use tempfile::tempdir;

    #[test]
    #[serial]
    fn capture_and_cleanup_round_trip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("owned.sock");
        let _listener = UnixListener::bind(&path).unwrap();

        let ownership = UnixSocketOwnership::capture(path.to_str().unwrap())
            .expect("capture must succeed right after bind");
        assert_eq!(ownership.cleanup_if_ours(), UnixSocketCleanup::Removed);
        assert!(!path.exists(), "our socket file must be removed");
    }

    #[test]
    #[serial]
    fn cleanup_skips_replaced_inode() {
        // Linux is free to recycle a freed inode immediately on tmpfs/ext4,
        // so bind→remove→bind on the same path can land on the same ino on
        // CI runners. We forge the mismatch directly: a stale ownership
        // claim against a live file is the same observable state the parent
        // would see after a successor rebound the socket.
        let dir = tempdir().unwrap();
        let path = dir.path().join("shared.sock");
        let live = UnixListener::bind(&path).unwrap();
        let real = UnixSocketOwnership::capture(path.to_str().unwrap()).unwrap();
        let stale = UnixSocketOwnership {
            path: real.path.clone(),
            dev: real.dev,
            ino: real.ino.wrapping_add(1),
        };

        match stale.cleanup_if_ours() {
            UnixSocketCleanup::Skipped { reason } => {
                assert!(
                    reason.contains("inode changed"),
                    "unexpected reason: {reason}"
                );
            }
            other => panic!("expected Skipped, got {other:?}"),
        }
        assert!(path.exists(), "live socket file must be preserved");
        drop(live);
    }

    #[test]
    #[serial]
    fn cleanup_reports_missing_when_already_removed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("gone.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        let ownership = UnixSocketOwnership::capture(path.to_str().unwrap()).unwrap();

        std::fs::remove_file(&path).unwrap();
        assert_eq!(ownership.cleanup_if_ours(), UnixSocketCleanup::Missing);
    }

    #[test]
    #[serial]
    fn inspect_remove_on_exact_match() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("inspect.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        let ownership = UnixSocketOwnership::capture(path.to_str().unwrap()).unwrap();

        assert_eq!(
            UnixSocketOwnership::inspect(path.to_str().unwrap(), ownership.dev, ownership.ino),
            CleanupDecision::Remove
        );
    }

    #[test]
    #[serial]
    fn inspect_skip_on_mismatched_ino() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("inspect2.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        let ownership = UnixSocketOwnership::capture(path.to_str().unwrap()).unwrap();

        // Pretend we captured a different inode to simulate replacement.
        let fake_ino = ownership.ino.wrapping_add(1);
        match UnixSocketOwnership::inspect(path.to_str().unwrap(), ownership.dev, fake_ino) {
            CleanupDecision::Skip(reason) => {
                assert!(reason.contains("inode changed"), "unexpected: {reason}");
            }
            other => panic!("expected Skip, got {other:?}"),
        }
    }

    #[test]
    #[serial]
    fn inspect_missing_when_no_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.sock");
        assert_eq!(
            UnixSocketOwnership::inspect(path.to_str().unwrap(), 0, 0),
            CleanupDecision::Missing
        );
    }
}

#[cfg(test)]
mod prepare_unix_socket_path_tests {
    use super::prepare_unix_socket_path;
    use serial_test::serial;
    use std::os::unix::net::UnixListener;
    use tempfile::tempdir;

    #[test]
    #[serial]
    fn missing_path_is_ok() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.sock");
        assert!(prepare_unix_socket_path(path.to_str().unwrap()).is_ok());
    }

    #[test]
    #[serial]
    fn stale_file_is_removed() {
        // A regular file (not a live listener) simulates a post-crash leftover
        // — prepare_unix_socket_path should clean it up silently.
        let dir = tempdir().unwrap();
        let path = dir.path().join("stale.sock");
        std::fs::write(&path, b"leftover").unwrap();
        assert!(path.exists());

        prepare_unix_socket_path(path.to_str().unwrap()).expect("stale file must be removable");
        assert!(!path.exists(), "stale socket file must be removed");
    }

    #[test]
    #[serial]
    fn live_listener_is_preserved() {
        // Bind a real UnixListener in a temp dir; the helper must refuse to
        // touch it and return a descriptive error.
        let dir = tempdir().unwrap();
        let path = dir.path().join("live.sock");
        let _listener = UnixListener::bind(&path).unwrap();

        let err = prepare_unix_socket_path(path.to_str().unwrap())
            .expect_err("live socket must trigger an error");
        assert!(err.contains("already listening"), "unexpected error: {err}");
        assert!(path.exists(), "live socket file must stay on disk");
    }
}

#[cfg(test)]
mod umask_guard_tests {
    use super::UmaskGuard;
    use serial_test::serial;

    #[test]
    #[serial]
    fn restore_previous_umask_on_drop() {
        let prior = unsafe { libc::umask(0o022) };
        {
            let _guard = UmaskGuard::restrict(0o077);
            let inside = unsafe { libc::umask(0o777) };
            unsafe { libc::umask(inside) };
            assert_eq!(
                inside & 0o077,
                0o077,
                "guard must ensure the restrict bits are set"
            );
        }
        let after = unsafe { libc::umask(0o022) };
        assert_eq!(after, 0o022, "drop must restore the original umask");
        unsafe { libc::umask(prior) };
    }

    #[test]
    #[serial]
    fn restrict_preserves_existing_bits() {
        let prior = unsafe { libc::umask(0o027) };
        {
            let _guard = UmaskGuard::restrict(0o050);
            let inside = unsafe { libc::umask(0o777) };
            unsafe { libc::umask(inside) };
            // Prior bits (027) AND new bits (050) must both be present.
            assert_eq!(inside & 0o077, 0o077);
        }
        unsafe { libc::umask(prior) };
    }
}

#[cfg(all(test, not(windows)))]
mod inherited_fd_cleanup_tests {
    use super::close_unexpected_fds;
    #[cfg(target_os = "linux")]
    use super::close_unexpected_fds_below_limit;
    #[cfg(target_os = "linux")]
    use serial_test::serial;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FdIdentity {
        dev: libc::dev_t,
        ino: libc::ino_t,
    }

    fn fd_identity(fd: libc::c_int) -> Option<FdIdentity> {
        // SAFETY: fstat reads metadata for the supplied descriptor only.
        unsafe {
            let mut stat = std::mem::zeroed::<libc::stat>();
            if libc::fstat(fd, &mut stat) == 0 {
                Some(FdIdentity {
                    dev: stat.st_dev,
                    ino: stat.st_ino,
                })
            } else {
                None
            }
        }
    }

    struct Pipe {
        read: libc::c_int,
        write: libc::c_int,
    }

    impl Pipe {
        fn new() -> Self {
            let mut fds = [0_i32; 2];
            let r = unsafe { libc::pipe(fds.as_mut_ptr()) };
            assert_eq!(r, 0, "pipe(2) failed: {}", std::io::Error::last_os_error());
            Self {
                read: fds[0],
                write: fds[1],
            }
        }

        fn mark_closed(&mut self) {
            self.read = -1;
            self.write = -1;
        }
    }

    impl Drop for Pipe {
        fn drop(&mut self) {
            if self.read >= 0 {
                unsafe { libc::close(self.read) };
            }
            if self.write >= 0 {
                unsafe { libc::close(self.write) };
            }
        }
    }

    #[test]
    fn close_unexpected_fds_preserves_allowlist() {
        let keep = Pipe::new();
        let mut leaked = Pipe::new();
        let keep_read_identity = fd_identity(keep.read).expect("keep read fd must be open");
        let keep_write_identity = fd_identity(keep.write).expect("keep write fd must be open");
        let leaked_read = leaked.read;
        let leaked_write = leaked.write;

        let mut allow = vec![0, 1, 2, keep.read, keep.write];
        allow.sort_unstable();
        let closed =
            close_unexpected_fds([keep.read, keep.write, leaked_read, leaked_write], &allow);
        leaked.mark_closed();

        assert_eq!(closed, 2, "expected to close exactly the leaked pipe fds");
        assert_eq!(
            fd_identity(keep.read),
            Some(keep_read_identity),
            "allowlisted read fd must survive"
        );
        assert_eq!(
            fd_identity(keep.write),
            Some(keep_write_identity),
            "allowlisted write fd must survive"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[serial]
    fn inherited_fd_cleanup_closes_open_fds_above_soft_limit() {
        let pipe = Pipe::new();
        let original_limit = unsafe {
            let mut rl = std::mem::zeroed::<libc::rlimit>();
            assert_eq!(
                libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl),
                0,
                "getrlimit failed: {}",
                std::io::Error::last_os_error()
            );
            rl
        };

        if original_limit.rlim_cur <= 64 {
            return;
        }

        let dup_fd = unsafe { libc::fcntl(pipe.read, libc::F_DUPFD_CLOEXEC, 64) };
        if dup_fd < 0 {
            return;
        }

        struct RlimitGuard(libc::rlimit);
        impl Drop for RlimitGuard {
            fn drop(&mut self) {
                unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) };
            }
        }
        let _guard = RlimitGuard(original_limit);

        let lowered = libc::rlimit {
            rlim_cur: 32,
            rlim_max: original_limit.rlim_max,
        };
        assert_eq!(
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lowered) },
            0,
            "setrlimit lower failed: {}",
            std::io::Error::last_os_error()
        );

        let mut keep: Vec<libc::c_int> = std::fs::read_dir("/proc/self/fd")
            .expect("read /proc/self/fd")
            .filter_map(|entry| {
                entry
                    .ok()
                    .and_then(|entry| entry.file_name().to_string_lossy().parse().ok())
            })
            .filter(|fd| *fd != dup_fd)
            .collect();
        keep.sort_unstable();
        keep.dedup();

        let closed = close_unexpected_fds_below_limit(&keep);

        assert!(
            closed >= 1,
            "cleanup must count high-numbered inherited fd closed above the soft limit"
        );
        assert_eq!(
            fd_identity(dup_fd),
            None,
            "cleanup must close unexpected inherited fd above current RLIMIT_NOFILE"
        );
    }
}

#[cfg(all(test, not(windows)))]
mod inherited_listener_tests {
    use super::{
        adopt_inherited_tcp_listener, adopt_inherited_unix_listener, drop_listener_owner,
        UnixSocketModeRollback, UnixSocketOwnership,
    };
    use serial_test::serial;
    use std::fs::File;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    #[test]
    fn rejects_invalid_inherited_listener_fd_without_panic() {
        let expected_addr = "127.0.0.1:1".parse().unwrap();
        let err = adopt_inherited_tcp_listener(-1, expected_addr).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn rejects_inherited_listener_bound_to_unexpected_addr() {
        let expected_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let expected_addr = expected_listener.local_addr().unwrap();
        let wrong_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let wrong_fd = unsafe { libc::dup(wrong_listener.as_raw_fd()) };
        assert!(
            wrong_fd >= 0,
            "dup failed: {}",
            std::io::Error::last_os_error()
        );

        let err = adopt_inherited_tcp_listener(wrong_fd, expected_addr).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("expected"),
            "wrong listener error should include expected addr, got: {msg}"
        );
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    #[serial]
    async fn adopts_inherited_unix_listener_bound_to_expected_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".s.PGSQL.6543");
        let listener = UnixListener::bind(&path).unwrap();
        let ownership = UnixSocketOwnership::capture(path.to_str().unwrap()).unwrap();
        let fd = unsafe { libc::dup(listener.as_raw_fd()) };
        assert!(fd >= 0, "dup failed: {}", std::io::Error::last_os_error());

        let adopted = adopt_inherited_unix_listener(
            fd,
            path.to_str().unwrap(),
            0o600,
            Some((ownership.dev, ownership.ino)),
        )
        .unwrap();

        assert!(adopted.local_addr().unwrap().as_pathname().is_some());
    }

    #[tokio::test]
    #[serial]
    async fn inherited_unix_listener_preserves_prepared_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".s.PGSQL.6545");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let ownership = UnixSocketOwnership::capture(path.to_str().unwrap()).unwrap();
        let fd = unsafe { libc::dup(listener.as_raw_fd()) };
        assert!(fd >= 0, "dup failed: {}", std::io::Error::last_os_error());

        let adopted = adopt_inherited_unix_listener(
            fd,
            path.to_str().unwrap(),
            0o600,
            Some((ownership.dev, ownership.ino)),
        )
        .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(adopted);
    }

    #[tokio::test]
    #[serial]
    async fn inherited_unix_listener_rejects_replaced_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".s.PGSQL.6547");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let ownership = UnixSocketOwnership::capture(path.to_str().unwrap()).unwrap();
        let fd = unsafe { libc::dup(listener.as_raw_fd()) };
        assert!(fd >= 0, "dup failed: {}", std::io::Error::last_os_error());

        std::fs::remove_file(&path).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();

        let err = adopt_inherited_unix_listener(
            fd,
            path.to_str().unwrap(),
            0o600,
            Some((ownership.dev, ownership.ino)),
        )
        .unwrap_err();

        let replacement_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("no longer names"),
            "replacement-path error should explain the ownership mismatch, got: {err}"
        );
        assert_eq!(
            replacement_mode, 0o666,
            "adopting the inherited fd must not chmod a replacement socket path"
        );
        drop(replacement);
    }

    #[test]
    fn unix_socket_mode_rollback_restores_original_mode_on_abort() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();

        let guard = UnixSocketModeRollback::tighten_if_needed(
            file.as_file().as_raw_fd(),
            path.to_str().unwrap(),
            0o600,
        )
        .unwrap()
        .expect("weak socket mode should be tightened before child spawn");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        drop(guard);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o666);
    }

    #[test]
    #[serial]
    fn unix_socket_mode_rollback_restores_fd_not_replaced_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".s.PGSQL.6548");
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();

        let guard = match UnixSocketModeRollback::tighten_if_needed(
            listener.as_raw_fd(),
            path.to_str().unwrap(),
            0o600,
        ) {
            Ok(Some(guard)) => guard,
            Ok(None) => panic!("weak socket mode should require tightening before child spawn"),
            Err(err) if err.raw_os_error() == Some(libc::EINVAL) => {
                let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
                assert_eq!(
                    mode, 0o666,
                    "unsupported fd chmod must not fall back to path chmod"
                );
                return;
            }
            Err(err) => panic!("unexpected socket mode tightening error: {err}"),
        };

        std::fs::remove_file(&path).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        drop(guard);

        let replacement_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            replacement_mode, 0o600,
            "rollback must not restore permissions on a replacement socket path"
        );
        drop(listener);
        drop(replacement);
    }

    #[test]
    fn unix_socket_mode_rollback_does_not_loosen_before_spawn() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_path_buf();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let guard = UnixSocketModeRollback::tighten_if_needed(
            file.as_file().as_raw_fd(),
            path.to_str().unwrap(),
            0o666,
        )
        .expect("metadata lookup should succeed");

        assert!(
            guard.is_none(),
            "pre-spawn parent must not loosen an already stricter Unix socket"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    #[serial]
    fn rejects_inherited_unix_listener_bound_to_unexpected_path() {
        let dir = tempfile::tempdir().unwrap();
        let actual_path = dir.path().join(".s.PGSQL.6543");
        let expected_path = dir.path().join(".s.PGSQL.6544");
        let listener = UnixListener::bind(&actual_path).unwrap();
        let fd = unsafe { libc::dup(listener.as_raw_fd()) };
        assert!(fd >= 0, "dup failed: {}", std::io::Error::last_os_error());

        let err = adopt_inherited_unix_listener(fd, expected_path.to_str().unwrap(), 0o600, None)
            .unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("expected"),
            "wrong Unix listener error should include expected path, got: {err}"
        );
    }

    #[tokio::test]
    async fn drop_listener_owner_clears_option_before_fd_reuse() {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std_listener.set_nonblocking(true).unwrap();
        let mut listener = Some(tokio::net::TcpListener::from_std(std_listener).unwrap());

        drop_listener_owner(&mut listener);

        assert!(listener.is_none());

        let file = File::open("/dev/null").unwrap();
        let file_fd = file.as_raw_fd();
        drop_listener_owner(&mut listener);
        assert!(
            unsafe { libc::fcntl(file_fd, libc::F_GETFD) } >= 0,
            "dropping an already-cleared listener must not close an unrelated reused fd"
        );
    }
}

#[cfg(test)]
mod migration_capacity_tests {
    use super::{MIGRATION_CHANNEL_CAPACITY_MAX, MIGRATION_QUEUED_PAYLOAD_HEAP_BUDGET_BYTES};
    use crate::client::migration::MAX_MIGRATION_PAYLOAD_BYTES;

    #[test]
    fn migration_channel_capacity_bounds_worst_case_queued_payload_heap() {
        let capacity = MIGRATION_CHANNEL_CAPACITY_MAX;
        let max_payload = MAX_MIGRATION_PAYLOAD_BYTES;
        let heap_budget = MIGRATION_QUEUED_PAYLOAD_HEAP_BUDGET_BYTES;
        let queued_payload = capacity * max_payload;
        assert!(
            queued_payload <= heap_budget,
            "migration channel can queue up to {queued_payload} worst-case payload bytes"
        );
    }
}

#[cfg(test)]
mod shutdown_timeout_reload_tests {
    #[test]
    fn shutdown_signals_read_live_shutdown_timeout_config() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);

        assert!(
            !impl_src.contains("let shutdown_timeout = config.general.shutdown_timeout.as_std();"),
            "run_server must not capture shutdown_timeout at startup; RELOAD publishes a new value"
        );

        let foreground_start = impl_src
            .find("Got SIGINT (Ctrl+C), starting graceful shutdown")
            .expect("foreground SIGINT shutdown branch not found");
        let foreground_block = &impl_src[foreground_start..];
        let foreground_end = foreground_block
            .find("spawn_shutdown_timer(exit_tx.clone(), shutdown_timeout)")
            .expect("foreground SIGINT branch should arm shutdown");
        let foreground_block = &foreground_block[..foreground_end];
        assert!(
            foreground_block.contains("let shutdown_timeout = live_shutdown_timeout();"),
            "foreground SIGINT shutdown must use the latest reloaded shutdown_timeout"
        );

        let binary_start = impl_src
            .find("Got SIGINT, starting binary upgrade and graceful shutdown")
            .expect("SIGINT binary-upgrade branch not found");
        let binary_block = &impl_src[binary_start..];
        let binary_end = binary_block
            .find(").await {")
            .expect("SIGINT binary-upgrade call not found");
        let binary_block = &binary_block[..binary_end];
        assert!(
            binary_block.contains("let shutdown_timeout = live_shutdown_timeout();"),
            "SIGINT binary upgrade must use the latest reloaded shutdown_timeout"
        );

        let sigusr2_start = impl_src
            .find("Got SIGUSR2, starting binary upgrade and graceful shutdown")
            .expect("SIGUSR2 binary-upgrade branch not found");
        let sigusr2_block = &impl_src[sigusr2_start..];
        let sigusr2_end = sigusr2_block
            .find(").await {")
            .expect("SIGUSR2 binary-upgrade call not found");
        let sigusr2_block = &sigusr2_block[..sigusr2_end];
        assert!(
            sigusr2_block.contains("let shutdown_timeout = live_shutdown_timeout();"),
            "SIGUSR2 binary upgrade must use the latest reloaded shutdown_timeout"
        );
    }

    #[test]
    fn shutdown_awaits_migration_sender_with_timeout() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let sender_wait_start = impl_src
            .find("if let Some(handles) = _migration_handles.take()")
            .expect("migration sender shutdown wait block not found");
        let block = &impl_src[sender_wait_start..];
        let block_end = block
            .find("drain in-flight graceful Terminate tasks")
            .expect("terminate task drain comment should follow sender wait block");
        let block = &block[..block_end];

        assert!(
            block.contains("tokio::time::timeout"),
            "shutdown must not await migration sender forever if the child stops draining"
        );
        assert!(
            block.contains("handles.sender_handle"),
            "shutdown timeout must wrap the migration sender JoinHandle"
        );
    }
}

#[cfg(test)]
mod binary_upgrade_spawn_tests {
    use super::{
        capture_unready_daemon_identity_from_pid_file, terminate_unready_daemon_from_pid_file,
        DaemonSuccessorIdentityCapture,
    };

    #[test]
    fn binary_upgrade_preflight_uses_parsed_config_file() {
        let src = include_str!("server.rs");
        let validation_start = src
            .find("First, validate configuration of the new binary")
            .expect("binary-upgrade validation block not found");
        let block = &src[validation_start..];
        let block_end = block
            .find("Successor spawn/readiness can still rollback below")
            .expect("binary-upgrade validation block end not found");
        let block = &block[..block_end];

        assert!(
            block.contains(".arg(&args.config_file)"),
            "binary-upgrade preflight must validate the parsed config_file, not rediscover a positional argv"
        );
        assert!(
            !block.contains(".find(|arg| !arg.starts_with('-'))"),
            "binary-upgrade preflight must not treat --inherit-fd values as config paths"
        );
    }

    #[test]
    fn binary_upgrade_notifies_stopping_after_successor_readiness() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let fn_start = impl_src
            .find("async fn binary_upgrade_and_shutdown")
            .expect("binary-upgrade function not found");
        let block = &impl_src[fn_start..];
        let daemon_start = block
            .find("Daemon mode: start new daemon process")
            .expect("daemon branch not found");
        let foreground_start = block
            .find("Foreground mode: start new process")
            .expect("foreground branch not found");
        let daemon_block = &block[daemon_start..foreground_start];
        let foreground_block = &block[foreground_start..];

        assert!(
            !block[..daemon_start].contains("notify_systemd_binary_upgrade_stopping"),
            "binary upgrade must not notify STOPPING before post-preflight spawn/readiness abort paths"
        );

        let daemon_ready_idx = daemon_block
            .find("New daemon process signaled readiness")
            .expect("daemon readiness success marker not found");
        let daemon_stopping_idx = daemon_block
            .find("notify_systemd_binary_upgrade_stopping")
            .expect("daemon success path must notify systemd STOPPING");
        let daemon_release_idx = daemon_block
            .find("drop_listener_owner(listener)")
            .expect("daemon success path must release listener");
        assert!(
            daemon_ready_idx < daemon_stopping_idx && daemon_stopping_idx < daemon_release_idx,
            "daemon binary upgrade must notify STOPPING after successor readiness and before listener release"
        );

        let foreground_ready_idx = foreground_block
            .find("New process signaled readiness")
            .expect("foreground readiness success marker not found");
        let foreground_stopping_idx = foreground_block
            .find("notify_systemd_binary_upgrade_stopping")
            .expect("foreground success path must notify systemd STOPPING");
        let foreground_release_idx = foreground_block
            .find("drop_listener_owner(listener)")
            .expect("foreground success path must release listener");
        assert!(
            foreground_ready_idx < foreground_stopping_idx
                && foreground_stopping_idx < foreground_release_idx,
            "foreground binary upgrade must notify STOPPING after successor readiness and before listener release"
        );
    }

    #[test]
    fn binary_upgrade_publishes_client_shutdown_after_successor_readiness() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let fn_start = impl_src
            .find("async fn binary_upgrade_and_shutdown")
            .expect("binary-upgrade function not found");
        let block = &impl_src[fn_start..];
        let daemon_start = block
            .find("Daemon mode: start new daemon process")
            .expect("daemon branch not found");
        let foreground_start = block
            .find("Foreground mode: start new process")
            .expect("foreground branch not found");

        assert!(
            !block[..daemon_start].contains("SHUTDOWN_IN_PROGRESS.store(true"),
            "binary upgrade must not publish client shutdown before successor readiness can still rollback"
        );

        let daemon_block = &block[daemon_start..foreground_start];
        let daemon_ready_idx = daemon_block
            .find("New daemon process signaled readiness")
            .expect("daemon readiness success marker not found");
        let daemon_shutdown_idx = daemon_block
            .find("SHUTDOWN_IN_PROGRESS.store(true")
            .expect("daemon success path must publish client shutdown");
        let daemon_stopping_idx = daemon_block
            .find("notify_systemd_binary_upgrade_stopping")
            .expect("daemon success path must notify systemd STOPPING");
        assert!(
            daemon_ready_idx < daemon_shutdown_idx && daemon_shutdown_idx < daemon_stopping_idx,
            "daemon client shutdown must be visible only after successor readiness and before STOPPING"
        );

        let foreground_block = &block[foreground_start..];
        let foreground_ready_idx = foreground_block
            .find("New process signaled readiness")
            .expect("foreground readiness success marker not found");
        let foreground_shutdown_idx = foreground_block
            .find("SHUTDOWN_IN_PROGRESS.store(true")
            .expect("foreground success path must publish client shutdown");
        let foreground_migration_idx = foreground_block
            .find("publish_migration_in_progress(true)")
            .expect("foreground migration path must publish migration before notifying clients");
        let foreground_notify_idx = foreground_block
            .find("MIGRATION_NOTIFY.notify_waiters()")
            .expect("foreground migration path must notify idle clients");
        assert!(
            foreground_ready_idx < foreground_shutdown_idx,
            "foreground client shutdown must not be visible before readiness rollback is impossible"
        );
        assert!(
            foreground_migration_idx < foreground_shutdown_idx
                && foreground_shutdown_idx < foreground_notify_idx,
            "foreground migration clients must see migration mode before shutdown notification wakes them"
        );
    }

    #[test]
    fn foreground_upgrade_spawn_does_not_unwrap_current_dir() {
        let src = include_str!("server.rs");
        let foreground_start = src
            .find("Foreground mode: start new process with inherited listener fd")
            .expect("foreground binary-upgrade spawn block not found");
        let block = &src[foreground_start..];
        let block_end = block
            .find("match child_result")
            .expect("foreground spawn result handling not found");
        let block = &block[..block_end];

        assert!(
            !block.contains("current_dir(std::env::current_dir().unwrap())"),
            "foreground binary upgrade must not panic if the deploy cwd was removed"
        );
        assert!(
            block.contains("unwrap_or_else"),
            "foreground binary upgrade should fall back or abort cleanly on current_dir failure"
        );
    }

    #[test]
    fn daemon_upgrade_waits_for_successor_readiness_before_releasing_listener() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let daemon_start = impl_src
            .find("Daemon mode: start new daemon process")
            .expect("daemon binary-upgrade spawn block not found");
        let block = &impl_src[daemon_start..];
        let block_end = block
            .find("} else {\n            // Foreground mode:")
            .expect("foreground branch should follow daemon branch");
        let block = &block[..block_end];

        assert!(
            block.contains("PG_DOORMAN_READY_FD"),
            "daemon binary upgrade must pass a readiness fd to the successor"
        );
        let wait_idx = block
            .find("wait_for_pipe_readiness")
            .expect("daemon binary upgrade must wait for successor readiness");
        let release_idx = block
            .find("drop_listener_owner(listener)")
            .expect("daemon binary upgrade must release the old listener only after readiness");
        assert!(
            wait_idx < release_idx,
            "daemon binary upgrade must wait for the successor readiness byte before releasing the old listener"
        );
        assert!(
            !block.contains("tokio::time::sleep(tokio::time::Duration::from_secs(1)).await"),
            "a fixed sleep is not a readiness handshake"
        );
    }

    #[test]
    fn daemon_upgrade_passes_tcp_listener_fd_to_successor() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let daemon_start = impl_src
            .find("Daemon mode: start new daemon process")
            .expect("daemon binary-upgrade spawn block not found");
        let block = &impl_src[daemon_start..];
        let block_end = block
            .find("} else {\n            // Foreground mode:")
            .expect("foreground branch should follow daemon branch");
        let block = &block[..block_end];

        assert!(
            block.contains(".arg(\"--inherit-fd\")"),
            "daemon binary upgrade must pass the TCP listener fd to the successor"
        );
        assert!(
            block.contains(".arg(listener_fd.to_string())"),
            "daemon binary upgrade must pass the captured TCP listener fd value"
        );
        assert!(
            block.contains("libc::fcntl(listener_fd, libc::F_SETFD, 0)"),
            "daemon successor pre_exec must clear FD_CLOEXEC on the inherited TCP listener fd"
        );
    }

    #[test]
    fn daemon_upgrade_passes_pid_file_fd_to_successor() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let daemon_start = impl_src
            .find("Daemon mode: start new daemon process")
            .expect("daemon binary-upgrade spawn block not found");
        let block = &impl_src[daemon_start..];
        let block_end = block
            .find("} else {\n            // Foreground mode:")
            .expect("foreground branch should follow daemon branch");
        let block = &block[..block_end];

        assert!(
            block.contains("DAEMON_PID_FILE_FD_ENV"),
            "daemon binary upgrade must pass the locked pid-file fd to the successor"
        );
        assert!(
            block.contains("daemon_pid_file_fd"),
            "daemon binary upgrade must capture the current locked pid-file fd"
        );
        assert!(
            block.contains("libc::fcntl(daemon_pid_file_fd, libc::F_SETFD, 0)"),
            "daemon successor pre_exec must clear FD_CLOEXEC on the inherited pid-file fd"
        );
    }

    #[test]
    fn daemon_upgrade_passes_trusted_identity_fd_to_successor() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let daemon_start = impl_src
            .find("Daemon mode: start new daemon process")
            .expect("daemon binary-upgrade spawn block not found");
        let block = &impl_src[daemon_start..];
        let block_end = block
            .find("} else {\n            // Foreground mode:")
            .expect("foreground branch should follow daemon branch");
        let block = &block[..block_end];

        assert!(
            block.contains("DAEMON_IDENTITY_FD_ENV"),
            "daemon binary upgrade must pass a trusted identity pipe to the successor"
        );
        assert!(
            block.contains("DaemonSuccessorIdentityCapture::start_from_fd"),
            "daemon rollback identity capture must read the trusted daemon identity pipe"
        );
        assert!(
            block.contains("libc::fcntl(daemon_identity_write_fd, libc::F_SETFD, 0)"),
            "daemon successor pre_exec must clear FD_CLOEXEC on the trusted identity fd"
        );
    }

    #[test]
    fn daemon_upgrade_spawn_error_resets_shutdown_flags() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let daemon_start = impl_src
            .find("Daemon mode: start new daemon process")
            .expect("daemon binary-upgrade spawn block not found");
        let block = &impl_src[daemon_start..];
        let block_end = block
            .find("} else {\n            // Foreground mode:")
            .expect("foreground branch should follow daemon branch");
        let block = &block[..block_end];
        let spawn_match_start = block
            .find("let mut child = match spawn_res")
            .expect("daemon spawn result handling not found");
        let spawn_match = &block[spawn_match_start..];
        let err_start = spawn_match
            .find("Err(e) => {")
            .expect("daemon spawn error branch not found");
        let err_block = &spawn_match[err_start..];
        let err_end = err_block
            .find("return migration_handles;")
            .expect("daemon spawn error branch must return without shutting down");
        let err_block = &err_block[..err_end];

        assert!(
            err_block.contains("publish_migration_in_progress(false)"),
            "daemon spawn error must clear MIGRATION_IN_PROGRESS before returning"
        );
        assert!(
            err_block.contains("SHUTDOWN_IN_PROGRESS.store(false"),
            "daemon spawn error must clear SHUTDOWN_IN_PROGRESS before returning"
        );
    }

    #[test]
    fn daemon_upgrade_timeout_runs_pid_file_successor_cleanup_before_rollback() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let daemon_start = impl_src
            .find("Daemon mode: start new daemon process")
            .expect("daemon binary-upgrade spawn block not found");
        let block = &impl_src[daemon_start..];
        let block_end = block
            .find("} else {\n            // Foreground mode:")
            .expect("foreground branch should follow daemon branch");
        let block = &block[..block_end];

        let before_spawn_idx = block
            .find("let daemon_pid_before_spawn = Some(std::process::id() as libc::pid_t)")
            .expect("daemon branch must snapshot the pre-spawn daemon identity");
        let timeout_idx = block
            .find("New daemon process did not signal readiness within 10s")
            .expect("daemon timeout branch not found");
        let terminate_idx = block
            .find("terminate_unready_daemon_from_pid_file(")
            .expect("daemon timeout must terminate a detached successor from pid file");

        assert!(
            before_spawn_idx < timeout_idx && timeout_idx < terminate_idx,
            "daemon timeout cleanup must compare against the pre-spawn daemon identity \
             and run fail-closed pid-file cleanup before rollback"
        );
    }

    #[test]
    fn daemon_upgrade_timeout_restores_old_daemon_pid_file() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let daemon_start = impl_src
            .find("Daemon mode: start new daemon process")
            .expect("daemon binary-upgrade spawn block not found");
        let block = &impl_src[daemon_start..];
        let block_end = block
            .find("} else {\n            // Foreground mode:")
            .expect("foreground branch should follow daemon branch");
        let block = &block[..block_end];

        let timeout_idx = block
            .find("New daemon process did not signal readiness within 10s")
            .expect("daemon timeout branch not found");
        let timeout_block = &block[timeout_idx..];
        let terminate_idx = timeout_block
            .find("terminate_unready_daemon_from_pid_file(")
            .expect("daemon timeout must terminate a detached successor from pid file");
        let restore_idx = timeout_block
            .find("rewrite_current_pid_file")
            .expect("daemon timeout rollback must restore the old daemon pid file");
        let migration_reset_idx = timeout_block
            .find("publish_migration_in_progress(false)")
            .expect("daemon timeout must reset migration state");

        assert!(
            terminate_idx < restore_idx
                && restore_idx < migration_reset_idx,
            "daemon timeout rollback must run failed-successor cleanup, restore the old daemon pid file, then clear rollback state"
        );
    }

    #[test]
    fn daemon_upgrade_captures_successor_identity_during_readiness_window() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let daemon_start = impl_src
            .find("Daemon mode: start new daemon process")
            .expect("daemon binary-upgrade spawn block not found");
        let block = &impl_src[daemon_start..];
        let block_end = block
            .find("} else {\n            // Foreground mode:")
            .expect("foreground branch should follow daemon branch");
        let block = &block[..block_end];

        let capture_idx = block
            .find("DaemonSuccessorIdentityCapture::start")
            .expect("daemon rollback must start successor identity capture");
        let wait_idx = block
            .find("wait_for_pipe_readiness")
            .expect("daemon branch must wait for successor readiness");
        let finish_idx = block
            .find("successor_identity_capture.finish()")
            .expect("daemon rollback must finish identity capture after readiness wait");
        let terminate_idx = block
            .find("terminate_unready_daemon_from_pid_file(")
            .expect("daemon timeout must terminate a detached successor from pid file");
        let terminate_call = &block[terminate_idx..];
        let terminate_call_end = terminate_call
            .find(");")
            .expect("terminate call should be complete");
        let terminate_call = &terminate_call[..terminate_call_end];

        assert!(
            capture_idx < wait_idx && wait_idx < finish_idx && finish_idx < terminate_idx,
            "daemon rollback must finish successor identity capture before pid-file cleanup decides whether signaling is allowed"
        );
        assert!(
            terminate_call.contains("successor_identity"),
            "daemon rollback terminator must receive the captured successor identity"
        );
    }

    #[test]
    fn unready_daemon_pid_file_ignores_current_and_previous_pid() {
        let file = tempfile::NamedTempFile::new().expect("temp pid file");
        let current = std::process::id() as libc::pid_t;
        std::fs::write(file.path(), format!("{current}\n")).expect("write pid file");

        assert!(
            !terminate_unready_daemon_from_pid_file(
                file.path(),
                Some(current),
                current.saturating_add(1),
                None,
            ),
            "timeout cleanup must not signal the current daemon or the pre-spawn pid"
        );
    }

    #[test]
    fn unready_daemon_pid_file_refuses_pidfile_captured_successor_identity() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep child");
        let pid = child.id() as libc::pid_t;
        let file = tempfile::NamedTempFile::new().expect("temp pid file");
        std::fs::write(file.path(), format!("{pid}\n")).expect("write pid file");
        let identity =
            capture_unready_daemon_identity_from_pid_file(file.path(), None, pid.saturating_add(1))
                .expect("sleep child should have a capturable process identity");

        let terminated = terminate_unready_daemon_from_pid_file(
            file.path(),
            None,
            pid.saturating_add(1),
            Some(identity),
        );
        let child_still_running = child.try_wait().expect("poll child").is_none();

        if child_still_running {
            let _ = child.kill();
        }
        let _ = child.wait();

        assert!(
            !terminated,
            "pidfile-captured identity must not authorize signaling a detached successor"
        );
        assert!(
            child_still_running,
            "pidfile-captured identity must leave the candidate pid alive"
        );
    }

    #[test]
    fn unready_daemon_pid_file_accepts_trusted_pipe_successor_identity() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep child");
        let pid = child.id() as libc::pid_t;
        let file = tempfile::NamedTempFile::new().expect("temp pid file");
        std::fs::write(file.path(), format!("{pid}\n")).expect("write pid file");

        let mut pipe_fds = [0; 2];
        assert_eq!(
            unsafe { libc::pipe(pipe_fds.as_mut_ptr()) },
            0,
            "create trusted identity pipe"
        );
        let read_fd = pipe_fds[0];
        let write_fd = pipe_fds[1];
        let payload = format!("{pid}\n");
        assert_eq!(
            unsafe {
                libc::write(
                    write_fd,
                    payload.as_ptr() as *const libc::c_void,
                    payload.len(),
                )
            },
            payload.len() as isize,
            "write trusted pid"
        );
        unsafe {
            libc::close(write_fd);
        }

        let capture =
            DaemonSuccessorIdentityCapture::start_from_fd(read_fd, None, pid.saturating_add(1));
        let identity = capture
            .finish()
            .expect("trusted pipe should capture sleep child identity");

        assert!(
            terminate_unready_daemon_from_pid_file(
                file.path(),
                None,
                pid.saturating_add(1),
                Some(identity),
            ),
            "trusted pipe identity must authorize detached successor cleanup"
        );

        for _ in 0..50 {
            if child.try_wait().expect("poll child").is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let _ = child.kill();
        let _ = child.wait();
        panic!("trusted successor pid was not terminated");
    }

    #[test]
    fn unready_daemon_pid_file_rejects_world_writable_pid_file() {
        use std::os::unix::fs::PermissionsExt;

        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep child");
        let pid = child.id() as libc::pid_t;
        let file = tempfile::NamedTempFile::new().expect("temp pid file");
        std::fs::write(file.path(), format!("{pid}\n")).expect("write pid file");
        let mut perms = std::fs::metadata(file.path())
            .expect("pid metadata")
            .permissions();
        perms.set_mode(0o666);
        std::fs::set_permissions(file.path(), perms).expect("make pid file world-writable");

        assert!(
            !terminate_unready_daemon_from_pid_file(file.path(), None, pid.saturating_add(1), None),
            "timeout cleanup must not signal a pid from an unsafe pid file"
        );
        assert!(
            child.try_wait().expect("poll child").is_none(),
            "unsafe pid-file cleanup must leave the unrelated pid alive"
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn daemon_pid_file_cleanup_never_sigkills_by_reused_numeric_pid() {
        let src = include_str!("server.rs");
        let fn_start = src
            .find("fn terminate_unready_daemon_from_pid_file")
            .expect("daemon pid-file cleanup helper not found");
        let fn_body = &src[fn_start..];
        let fn_end = fn_body
            .find("\n}\n\n#[cfg")
            .expect("daemon pid-file cleanup helper end not found");
        let fn_body = &fn_body[..fn_end];

        assert!(
            !fn_body.contains("libc::kill(pid, libc::SIGKILL)"),
            "daemon rollback must not SIGKILL a bare numeric pid after a sleep; \
             the pid may have been reused after SIGTERM"
        );
    }

    #[test]
    fn linux_daemon_pidfd_open_failure_has_no_numeric_signal_fallback() {
        let src = include_str!("server.rs");
        let fn_start = src
            .find("fn terminate_unready_daemon_pid_with_pidfd")
            .expect("Linux daemon pidfd helper not found");
        let fn_body = &src[fn_start..];
        let fn_end = fn_body
            .find("\n}\n\n#[cfg(not(windows))]")
            .expect("Linux daemon pidfd helper end not found");
        let fn_body = &fn_body[..fn_end];

        assert!(
            !fn_body.contains("libc::kill(pid, libc::SIGTERM)"),
            "Linux daemon rollback must fail closed when pidfd_open fails; \
             a numeric SIGTERM can hit a reused pid"
        );
    }

    #[test]
    fn daemon_pid_file_cleanup_reads_pid_from_validated_fd() {
        let src = include_str!("server.rs");
        let fn_start = src
            .find("fn terminate_unready_daemon_from_pid_file")
            .expect("daemon pid-file cleanup helper not found");
        let fn_body = &src[fn_start..];
        let fn_end = fn_body
            .find("\n}\n\n/// Perform binary upgrade")
            .expect("daemon pid-file cleanup helper end not found");
        let fn_body = &fn_body[..fn_end];

        assert!(
            fn_body.contains("open_daemon_pid_file_for_signal"),
            "daemon rollback must open and validate the pid file once before signaling"
        );
        assert!(
            fn_body.contains("read_daemon_pid_file_from_open_file"),
            "daemon rollback must read the pid from the already-validated file descriptor"
        );
        assert!(
            !fn_body.contains("read_daemon_pid_file(pid_file.as_ref())")
                && !fn_body.contains("read_daemon_pid_file(&"),
            "daemon rollback must not re-open the pid-file path after validation"
        );
    }

    #[test]
    fn daemon_pid_file_signal_open_is_nonblocking_before_validation() {
        let src = include_str!("server.rs");
        let fn_start = src
            .find("fn open_daemon_pid_file_for_signal")
            .expect("daemon pid-file signal open helper not found");
        let fn_body = &src[fn_start..];
        let fn_end = fn_body
            .find("\n}\n\n#[cfg")
            .expect("daemon pid-file signal open helper end not found");
        let fn_body = &fn_body[..fn_end];

        assert!(
            fn_body.contains("libc::O_NONBLOCK"),
            "rollback pid-file open must be nonblocking so FIFO/device paths \
             are rejected after fd validation instead of blocking the SIGUSR2 path"
        );
    }

    #[test]
    fn daemon_upgrade_does_not_reread_pid_file_path_before_spawn() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let daemon_start = impl_src
            .find("Daemon mode: start new daemon process")
            .expect("daemon binary-upgrade spawn block not found");
        let block = &impl_src[daemon_start..];
        let block_end = block
            .find("} else {\n            // Foreground mode:")
            .expect("foreground branch should follow daemon branch");
        let block = &block[..block_end];

        assert!(
            !block.contains("read_daemon_pid_file(&daemon_pid_file)"),
            "daemon SIGUSR2 rollback must not read the pid-file path before \
             spawn; mutable paths can block or change before fd validation"
        );
        assert!(
            block.contains("std::process::id() as libc::pid_t"),
            "the pre-spawn daemon identity should come from the running process, not a path read"
        );
    }

    #[test]
    fn client_migration_mode_is_published_only_after_sender_channel() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let fn_start = impl_src
            .find("async fn binary_upgrade_and_shutdown")
            .expect("binary-upgrade function not found");
        let body = &impl_src[fn_start..];

        let migration_true_idx = body
            .find("publish_migration_in_progress(true)")
            .expect("client migration publication not found");
        let tx_set_idx = body
            .find("MIGRATION_TX.set(tx)")
            .expect("migration sender channel publication not found");
        let notify_idx = body
            .find("MIGRATION_NOTIFY.notify_waiters()")
            .expect("migration waiters notification not found");

        assert!(
            tx_set_idx < migration_true_idx,
            "client migration mode must not be visible before MIGRATION_TX is installed"
        );
        assert!(
            migration_true_idx < notify_idx,
            "migration waiters must be notified after client migration mode is visible"
        );
    }

    #[test]
    fn migration_mode_publication_uses_release_acquire_ordering() {
        let server_src = include_str!("server.rs");
        let server_impl = server_src
            .split("#[cfg(test)]")
            .next()
            .unwrap_or(server_src);
        let transaction_src = include_str!("../client/transaction.rs");
        let transaction_impl = transaction_src
            .split("#[cfg(test)]")
            .next()
            .unwrap_or(transaction_src);

        assert!(
            server_impl.contains("MIGRATION_IN_PROGRESS.store(in_progress, Ordering::Release)"),
            "publishing migration mode must use Release so clients that observe it \
             also observe the existing MIGRATION_TX channel"
        );
        assert!(
            server_impl.contains("MIGRATION_IN_PROGRESS.load(Ordering::Acquire)"),
            "migration mode readers must use Acquire before relying on MIGRATION_TX visibility"
        );
        assert!(
            !server_impl.contains("MIGRATION_IN_PROGRESS.store(true, Ordering::Relaxed)")
                && !server_impl.contains("MIGRATION_IN_PROGRESS.load(Ordering::Relaxed)")
                && !transaction_impl.contains("MIGRATION_IN_PROGRESS.load(Ordering::Relaxed)"),
            "migration mode must not be published or consumed with Relaxed ordering"
        );
    }

    #[test]
    fn foreground_upgrade_seeds_child_connection_counter_before_accept() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);

        let child_seed_idx = impl_src
            .find("PG_DOORMAN_MIGRATION_COUNTER")
            .expect("child startup must read the parent connection counter high-water mark");
        let receiver_idx = impl_src
            .find("migration_receiver_task(")
            .expect("child startup must spawn the migration receiver");
        let accepting_idx = impl_src
            .find("Accepting connections")
            .expect("child startup accept marker not found");
        assert!(
            child_seed_idx < receiver_idx && child_seed_idx < accepting_idx,
            "child must seed TOTAL_CONNECTION_COUNTER before migrated or fresh clients can register"
        );
        assert!(
            impl_src[child_seed_idx..receiver_idx].contains("TOTAL_CONNECTION_COUNTER.fetch_max"),
            "child startup must advance TOTAL_CONNECTION_COUNTER from the inherited high-water mark"
        );

        let foreground_start = impl_src
            .find("Foreground mode: start new process with inherited listener fd")
            .expect("foreground binary-upgrade spawn block not found");
        let foreground_block = &impl_src[foreground_start..];
        let spawn_end = foreground_block
            .find("match child_result")
            .expect("foreground spawn result handling not found");
        let spawn_block = &foreground_block[..spawn_end];
        assert!(
            spawn_block.contains("TOTAL_CONNECTION_COUNTER.load"),
            "foreground parent must snapshot the connection counter before spawning the child"
        );
        assert!(
            spawn_block.contains("PG_DOORMAN_MIGRATION_COUNTER"),
            "foreground parent must pass the connection counter high-water mark to the child"
        );
    }

    #[test]
    fn fresh_accepts_wait_for_migration_receiver_initial_drain() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);

        assert!(
            impl_src.contains("migration_receiver_active.store(true, Ordering::Release)"),
            "child startup must mark the migration receiver active before spawning it"
        );
        assert!(
            impl_src.contains("migration_fresh_accept_released.store(false, Ordering::Release)")
                && impl_src.contains("MIGRATION_FRESH_ACCEPT_GRACE"),
            "fresh accepts must wait for a bounded initial migration-drain grace window"
        );
        assert!(
            impl_src.contains("migration_receiver_active.store(false, Ordering::Release)")
                && impl_src
                    .contains("migration_fresh_accept_released.store(true, Ordering::Release)")
                && impl_src.contains("migration_receiver_drained.notify_waiters()"),
            "migration receiver task must wake fresh accepts after draining migrated fds"
        );

        let tcp_accept_idx = impl_src
            .find("let accept_future = async")
            .expect("TCP accept future not found");
        let tcp_accept_block = &impl_src[tcp_accept_idx..];
        let tcp_wait_idx = tcp_accept_block
            .find("wait_for_migration_receiver_drain(")
            .expect("TCP accept must wait for migration receiver drain");
        let tcp_accept_call_idx = tcp_accept_block
            .find("l.accept().await")
            .expect("TCP accept call not found");
        assert!(
            tcp_wait_idx < tcp_accept_call_idx,
            "TCP fresh accepts must wait for migrated clients before l.accept()"
        );

        let unix_accept_idx = impl_src
            .find("new_unix = async")
            .expect("Unix accept future not found");
        let unix_accept_block = &impl_src[unix_accept_idx..];
        let unix_wait_idx = unix_accept_block
            .find("wait_for_migration_receiver_drain(")
            .expect("Unix accept must wait for migration receiver drain");
        let unix_accept_call_idx = unix_accept_block
            .find("l.accept().await")
            .expect("Unix accept call not found");
        assert!(
            unix_wait_idx < unix_accept_call_idx,
            "Unix fresh accepts must wait for migrated clients before l.accept()"
        );
    }

    #[test]
    fn binary_upgrade_inherits_unix_listener_without_parent_cleanup_unlink() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let fn_start = impl_src
            .find("async fn binary_upgrade_and_shutdown")
            .expect("binary-upgrade function not found");
        let body = &impl_src[fn_start..];

        assert!(
            body.contains("unix_listener.as_ref().map(|l| l.as_raw_fd())"),
            "binary upgrade must capture the existing Unix listener fd before spawning the child"
        );
        assert!(
            body.contains(".arg(\"--inherit-unix-fd\")"),
            "binary upgrade must pass the Unix listener fd to the child"
        );
        assert!(
            body.contains("INHERITED_UNIX_SOCKET_DEV_ENV")
                && body.contains("INHERITED_UNIX_SOCKET_INO_ENV"),
            "binary upgrade must pass parent-captured Unix socket ownership to the child"
        );
        assert!(
            body.contains("libc::fcntl(unix_listener_fd"),
            "child pre_exec must clear FD_CLOEXEC on the inherited Unix listener fd"
        );
        assert!(
            body.contains("drop_unix_listener_owner(unix_listener)"),
            "parent must stop accepting on the inherited Unix listener after child readiness"
        );
        assert!(
            body.contains("let _ = unix_socket_ownership.take()"),
            "parent must not unlink the inherited Unix socket path during shutdown cleanup"
        );
    }

    #[test]
    fn binary_upgrade_tightens_unix_socket_mode_before_child_spawn() {
        let src = include_str!("server.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let fn_start = impl_src
            .find("async fn binary_upgrade_and_shutdown")
            .expect("binary-upgrade function not found");
        let body = &impl_src[fn_start..];

        let mode_guard_idx = body
            .find("prepare_inherited_unix_listener_mode_for_upgrade")
            .expect("binary upgrade must prepare inherited Unix socket mode in the parent");
        let daemon_idx = body
            .find("Daemon mode: start new daemon process")
            .expect("daemon branch not found");
        let foreground_idx = body
            .find("Foreground mode: start new process")
            .expect("foreground branch not found");

        assert!(
            mode_guard_idx < daemon_idx && mode_guard_idx < foreground_idx,
            "parent must prepare inherited Unix socket mode or abort before spawning the child"
        );
        assert!(
            body.contains("rollback.disarm()"),
            "parent must rollback the pre-spawn chmod on upgrade abort and disarm it after readiness"
        );
    }
}

#[cfg(test)]
mod cpu_affinity_tests {
    use super::select_worker_affinity_core;
    use crate::utils::core_affinity::CoreId;

    #[test]
    fn worker_affinity_core_selection_wraps_without_oob() {
        let core_ids = vec![CoreId { id: 0 }, CoreId { id: 1 }, CoreId { id: 2 }];

        let selected = select_worker_affinity_core(&core_ids, core_ids.len())
            .expect("three cores should allow affinity pinning");

        assert_eq!(selected.id, 0);
    }
}

#[cfg(all(test, not(windows)))]
mod wait_for_pipe_readiness_tests {
    use super::wait_for_pipe_readiness;

    /// Pipe wrapper that closes both ends, including high-fd dup tests.
    struct Pipe {
        read: libc::c_int,
        write: libc::c_int,
    }

    impl Pipe {
        fn new() -> Self {
            let mut fds = [0_i32; 2];
            let r = unsafe { libc::pipe(fds.as_mut_ptr()) };
            assert_eq!(r, 0, "pipe(2) failed: {}", std::io::Error::last_os_error());
            Self {
                read: fds[0],
                write: fds[1],
            }
        }
    }

    impl Drop for Pipe {
        fn drop(&mut self) {
            if self.read >= 0 {
                unsafe { libc::close(self.read) };
            }
            if self.write >= 0 {
                unsafe { libc::close(self.write) };
            }
        }
    }

    #[test]
    fn returns_false_on_timeout() {
        let pipe = Pipe::new();
        // 50 ms timeout, nothing was written.
        assert!(!wait_for_pipe_readiness(pipe.read, 50));
    }

    #[test]
    fn returns_true_when_byte_is_pending() {
        let pipe = Pipe::new();
        let byte: u8 = 1;
        let written =
            unsafe { libc::write(pipe.write, &byte as *const u8 as *const libc::c_void, 1) };
        assert_eq!(written, 1, "write to pipe failed");
        assert!(wait_for_pipe_readiness(pipe.read, 1_000));
    }

    /// EOF-only readiness must not count as child readiness.
    #[test]
    fn returns_false_when_writer_closes_without_writing() {
        let mut pipe = Pipe::new();
        unsafe { libc::close(pipe.write) };
        pipe.write = -1;
        assert!(!wait_for_pipe_readiness(pipe.read, 1_000));
    }

    /// Readiness polling must work for fds above `FD_SETSIZE`.
    #[test]
    fn handles_fd_above_fd_setsize() {
        let pipe = Pipe::new();
        // Pick a descriptor above select(2)'s usual 1023 ceiling.
        let target_fd: libc::c_int = 1500;
        let dup_result = unsafe { libc::dup2(pipe.read, target_fd) };
        if dup_result == -1 {
            // Not enough RLIMIT_NOFILE headroom in this runner.
            eprintln!(
                "skipping handles_fd_above_fd_setsize: dup2 to {target_fd} failed ({})",
                std::io::Error::last_os_error()
            );
            return;
        }
        // Pre-fill so poll has data to read immediately.
        let byte: u8 = 1;
        let written =
            unsafe { libc::write(pipe.write, &byte as *const u8 as *const libc::c_void, 1) };
        assert_eq!(written, 1, "write to pipe failed");

        // This call would panic on the pre-poll implementation.
        let ready = wait_for_pipe_readiness(target_fd, 1_000);

        // Always close the dup'd fd so the runner does not leak it.
        unsafe { libc::close(target_fd) };
        assert!(ready, "poll must observe POLLIN on a high-numbered fd");
    }
}
