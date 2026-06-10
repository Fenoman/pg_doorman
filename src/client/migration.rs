use bytes::{Buf, BufMut, BytesMut};
use log::{error, info, warn};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::sync::Arc;
use tokio::io::{split, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

#[cfg(feature = "tls-migration")]
use std::ffi::c_void;

use crate::client::buffer_pool::PooledBuffer;
use crate::client::core::{CachedStatement, Client, PreparedStatementKey, PreparedStatementKeyRef};
use crate::client::util::{
    extract_reset_cleanup_commands, extract_set_cleanup_commands, PREPARED_STATEMENT_COUNTER,
};
use crate::config::{get_config, BackendAuthMethod};
use crate::errors::Error;
use crate::messages::config_socket::configure_tcp_socket;
use crate::messages::Parse;
use crate::pool::{
    get_pool_by_id, resolve_client_anon_cache_size, ClientServerMap, ConnectionPool, PoolIdentifier,
};
use crate::server::ServerParameters;
use crate::stats::ClientStats;

use super::core::PreparedStatementState;

/// Restore migrated SCRAM state when backend auth is still pending.
fn restore_backend_auth_if_pending(
    pool: Option<&ConnectionPool>,
    migrated_auth: Option<&BackendAuthMethod>,
    migrated_pool_config_hash: Option<u64>,
    username: &str,
    pool_name: &str,
) {
    if let (Some(pool), Some(auth), Some(migrated_pool_config_hash)) =
        (pool, migrated_auth, migrated_pool_config_hash)
    {
        if pool.config_hash != migrated_pool_config_hash {
            warn!(
                "[{username}@{pool_name}] skipped migrated backend auth restore: \
                 pool config hash changed"
            );
            return;
        }
        if let Some(ref ba_lock) = pool.address.backend_auth {
            let needs_update = matches!(*ba_lock.read(), BackendAuthMethod::ScramPending);
            if needs_update {
                *ba_lock.write() = auth.clone();
                info!("[{username}@{pool_name}] restored backend auth from migrated client");
            }
        }
    }
}

fn ensure_migrated_pool_config_hash_matches(
    pool: Option<&ConnectionPool>,
    migrated_pool_config_hash: Option<u64>,
    username: &str,
    pool_name: &str,
) -> Result<(), Error> {
    let pool = pool.ok_or_else(|| {
        Error::ClientError(format!(
            "migration: pool {pool_name:?} for user {username:?} no longer exists"
        ))
    })?;
    let migrated_pool_config_hash = migrated_pool_config_hash.ok_or_else(|| {
        Error::ClientError(format!(
            "migration: missing pool config hash for pool {pool_name:?}, user {username:?}"
        ))
    })?;

    if pool.config_hash != migrated_pool_config_hash {
        warn!("[{username}@{pool_name}] rejected migrated client: pool config hash changed");
        return Err(Error::ClientError(format!(
            "migration: pool config changed for pool {pool_name:?}, user {username:?}; reconnect required"
        )));
    }

    Ok(())
}

const MIGRATION_MAGIC: u32 = 0x50474D47; // "PGMG"
const MIGRATION_VERSION: u16 = 2;
const MIGRATION_EXT_LAST_ANONYMOUS_HASH: u8 = 0xA1;
const MIGRATION_EXT_POOL_USER: u8 = 0xA2;
const MIGRATION_EXT_POOL_CONFIG_HASH: u8 = 0xA3;
/// Fixed-size header: magic(4) + version(2) + connection_id(8) + secret_key(4) + transaction_mode(1)
const HEADER_SIZE: usize = 4 + 2 + 8 + 4 + 1;
const MAX_PREPARED_ENTRIES: usize = 100_000;
const MAX_QUERY_LEN: usize = 10 * 1024 * 1024; // 10 MB
const MAX_RECV_BUF: usize = 64 * 1024;
const MAX_MIGRATION_STATE_LEN: usize = 64 * 1024 * 1024;
const MAX_MIGRATION_TLS_STATE_LEN: usize = 16 * 1024 * 1024;
pub(crate) const MAX_MIGRATION_PAYLOAD_BYTES: usize =
    MAX_MIGRATION_STATE_LEN + MAX_MIGRATION_TLS_STATE_LEN;

#[cfg(target_os = "linux")]
fn recvmsg_cloexec_flags() -> libc::c_int {
    libc::MSG_CMSG_CLOEXEC
}

#[cfg(not(target_os = "linux"))]
fn recvmsg_cloexec_flags() -> libc::c_int {
    0
}

fn set_close_on_exec(fd: RawFd) -> Result<(), Error> {
    // SAFETY: fcntl reads and writes descriptor flags for this fd.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return Err(Error::SocketError(format!(
                "fcntl(F_GETFD): {}",
                std::io::Error::last_os_error()
            )));
        }
        if flags & libc::FD_CLOEXEC != 0 {
            return Ok(());
        }
        if libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
            return Err(Error::SocketError(format!(
                "fcntl(F_SETFD FD_CLOEXEC): {}",
                std::io::Error::last_os_error()
            )));
        }
    }
    Ok(())
}

fn recvmsg_retrying_interrupted(
    socket_fd: RawFd,
    msghdr: &mut libc::msghdr,
) -> Result<isize, Error> {
    loop {
        // SAFETY: caller provides a valid msghdr pointing to live buffers.
        let n = unsafe { libc::recvmsg(socket_fd, msghdr, recvmsg_cloexec_flags()) };
        if n >= 0 {
            return Ok(n as isize);
        }

        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(Error::SocketError(format!("recvmsg: {err}")));
    }
}

fn recv_retrying_interrupted(
    socket_fd: RawFd,
    buf: &mut [u8],
    context: &'static str,
) -> Result<usize, Error> {
    loop {
        // SAFETY: caller provides a valid socket fd and writable buffer.
        let n = unsafe {
            libc::recv(
                socket_fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
            )
        };
        if n >= 0 {
            return Ok(n as usize);
        }

        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(Error::SocketError(format!("migration: {context}: {err}")));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MigrationReceiveErrorKind {
    Eof,
    Failure(&'static str),
}

fn migration_receive_error_kind(error: &Error) -> MigrationReceiveErrorKind {
    match error {
        Error::SocketError(message) if message == "migration socket closed" => {
            MigrationReceiveErrorKind::Eof
        }
        Error::SocketError(message) if message.starts_with("recvmsg:") => {
            MigrationReceiveErrorKind::Failure("recvmsg")
        }
        Error::SocketError(message) if message.contains("truncated") => {
            MigrationReceiveErrorKind::Failure("truncated_frame")
        }
        Error::SocketError(message) if message.contains("length") || message.contains("no fd") => {
            MigrationReceiveErrorKind::Failure("protocol")
        }
        Error::SocketError(_) => MigrationReceiveErrorKind::Failure("socket"),
        _ => MigrationReceiveErrorKind::Failure("other"),
    }
}

fn ensure_tcp_stream_fd(fd: RawFd, context: &str) -> Result<(), Error> {
    let mut socket_type: libc::c_int = 0;
    let mut socket_type_len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: getsockopt writes one c_int into socket_type when fd is a socket.
    let type_rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            &mut socket_type as *mut _ as *mut libc::c_void,
            &mut socket_type_len,
        )
    };
    if type_rc < 0 {
        return Err(Error::SocketError(format!(
            "{context}: getsockopt(SO_TYPE) failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    if socket_type != libc::SOCK_STREAM {
        return Err(Error::ClientError(format!(
            "{context}: non-TCP socket type {socket_type} cannot be migrated"
        )));
    }

    let mut addr: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut addr_len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: getsockname writes at most addr_len bytes into sockaddr_storage.
    let name_rc = unsafe {
        libc::getsockname(
            fd,
            &mut addr as *mut _ as *mut libc::sockaddr,
            &mut addr_len,
        )
    };
    if name_rc < 0 {
        return Err(Error::SocketError(format!(
            "{context}: getsockname failed: {}",
            std::io::Error::last_os_error()
        )));
    }

    match addr.ss_family as libc::c_int {
        libc::AF_INET | libc::AF_INET6 => Ok(()),
        family => Err(Error::ClientError(format!(
            "{context}: non-TCP socket family {family} cannot be migrated"
        ))),
    }
}

fn close_raw_fd(fd: RawFd) {
    if fd >= 0 {
        // SAFETY: caller passes an owned raw fd that must be closed on error.
        unsafe { libc::close(fd) };
    }
}

struct RawFdGuard {
    fd: RawFd,
}

impl RawFdGuard {
    fn new(fd: RawFd) -> Self {
        Self { fd }
    }

    fn into_raw_fd(mut self) -> RawFd {
        let fd = self.fd;
        self.fd = -1;
        fd
    }
}

impl Drop for RawFdGuard {
    fn drop(&mut self) {
        close_raw_fd(self.fd);
    }
}

// FFI for our patched OpenSSL migration functions.
// Only available with the tls-migration feature (vendored patched OpenSSL).
#[cfg(feature = "tls-migration")]
#[allow(dead_code)]
extern "C" {
    fn SSL_export_migration_state(ssl: *mut c_void, out: *mut *mut u8, out_len: *mut usize) -> i32;

    fn SSL_import_migration_state(
        ctx: *mut c_void,
        fd: i32,
        buf: *const u8,
        len: usize,
    ) -> *mut c_void;
}

/// Export TLS cipher state from a raw SSL* pointer.
#[cfg(feature = "tls-migration")]
fn export_tls_state_from_ptr(ssl_ptr: *mut c_void) -> Result<Vec<u8>, Error> {
    if ssl_ptr.is_null() {
        return Err(Error::ClientError("null SSL pointer".into()));
    }
    unsafe {
        let mut out: *mut u8 = std::ptr::null_mut();
        let mut out_len: usize = 0;
        // SAFETY: ssl_ptr belongs to the live TlsStream at the migration idle point.
        let ret = SSL_export_migration_state(ssl_ptr, &mut out, &mut out_len);
        if ret != 1 || out.is_null() {
            return Err(Error::ClientError(
                "SSL_export_migration_state failed".into(),
            ));
        }
        let data = std::slice::from_raw_parts(out, out_len).to_vec();
        openssl_sys::OPENSSL_free(out as *mut c_void);
        Ok(data)
    }
}

/// Payload sent over the migration socket.
/// Drop closes the dup'd fd if it was not consumed by sendmsg.
pub struct MigrationPayload {
    pub state: BytesMut,
    pub fd: RawFd,
    /// Opaque TLS cipher state from SSL_export_migration_state.
    /// None for plain TCP connections.
    pub tls_state: Option<Vec<u8>>,
}

impl Drop for MigrationPayload {
    fn drop(&mut self) {
        if self.fd >= 0 {
            // SAFETY: this struct owns the dup'd fd from prepare_migration.
            unsafe { libc::close(self.fd) };
        }
    }
}

// ---------------------------------------------------------------------------
// Serialization helpers
// ---------------------------------------------------------------------------

fn put_str(buf: &mut BytesMut, s: &str) -> Result<(), Error> {
    let len = u16::try_from(s.len()).map_err(|_| {
        Error::ClientError(format!(
            "migration: string length {} exceeds u16 frame limit",
            s.len()
        ))
    })?;
    buf.put_u16(len);
    buf.put_slice(s.as_bytes());
    Ok(())
}

fn checked_str_frame_len(s: &str) -> Result<usize, Error> {
    u16::try_from(s.len()).map_err(|_| {
        Error::ClientError(format!(
            "migration: string length {} exceeds u16 frame limit",
            s.len()
        ))
    })?;
    Ok(2 + s.len())
}

fn checked_migration_state_sum(parts: &[usize], context: &str) -> Result<usize, Error> {
    let mut total = 0usize;
    for part in parts {
        total = total.checked_add(*part).ok_or_else(|| {
            Error::ClientError(format!(
                "migration state length overflow while serializing {context}"
            ))
        })?;
        if total > MAX_MIGRATION_STATE_LEN {
            return Err(Error::ClientError(format!(
                "migration state length would exceed limit {MAX_MIGRATION_STATE_LEN} while serializing {context}"
            )));
        }
    }
    Ok(total)
}

fn ensure_migration_state_room(
    buf: &BytesMut,
    additional: usize,
    context: &str,
) -> Result<(), Error> {
    checked_migration_state_sum(&[buf.len(), additional], context).map(|_| ())
}

fn serialize_server_parameters(
    buf: &mut BytesMut,
    params: &std::collections::HashMap<String, String>,
) -> Result<(), Error> {
    let count = u16::try_from(params.len()).map_err(|_| {
        Error::ClientError(format!(
            "migration: server parameter count {} exceeds u16 frame limit",
            params.len()
        ))
    })?;

    let mut section_len = 2usize;
    for (key, value) in params {
        let entry_len = checked_migration_state_sum(
            &[checked_str_frame_len(key)?, checked_str_frame_len(value)?],
            "server parameter entry",
        )?;
        section_len = checked_migration_state_sum(&[section_len, entry_len], "server parameters")?;
    }
    ensure_migration_state_room(buf, section_len, "server parameters")?;

    buf.put_u16(count);
    for (key, value) in params {
        put_str(buf, key)?;
        put_str(buf, value)?;
    }
    Ok(())
}

fn get_str(buf: &mut impl Buf) -> Result<String, Error> {
    require(buf, 2)?;
    let len = buf.get_u16() as usize;
    require(buf, len)?;
    let mut v = vec![0u8; len];
    buf.copy_to_slice(&mut v);
    String::from_utf8(v).map_err(|_| Error::ClientError("migration: invalid utf8".into()))
}

/// Check that buf has at least `need` bytes remaining.
fn require(buf: &impl Buf, need: usize) -> Result<(), Error> {
    if buf.remaining() < need {
        return Err(Error::ClientError(format!(
            "migration: need {need} bytes, have {}",
            buf.remaining()
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Client → MigrationPayload
// ---------------------------------------------------------------------------

impl<S, T> Client<S, T>
where
    S: tokio::io::AsyncRead + std::marker::Unpin,
    T: tokio::io::AsyncWrite + std::marker::Unpin,
{
    /// Serialize idle client state and dup its socket fd for migration.
    pub fn prepare_migration(&self) -> Result<MigrationPayload, Error> {
        #[cfg(not(feature = "tls-migration"))]
        if self.stats.tls() {
            return Err(Error::ClientError(
                "TLS migration unavailable in this build".into(),
            ));
        }
        #[cfg(all(unix, feature = "tls-migration"))]
        if self.stats.tls() && self.ssl_ptr.is_none() {
            return Err(Error::ClientError(
                "TLS migration unavailable in this build".into(),
            ));
        }

        let raw_fd = self
            .raw_fd
            .ok_or_else(|| Error::ClientError("no raw_fd for migration".into()))?;
        ensure_tcp_stream_fd(raw_fd, "prepare migration")?;
        if self.migration_pool_is_dynamic {
            return Err(Error::ClientError(format!(
                "migration: dynamic auth_query pool {} cannot be migrated without child pool state",
                self.cached_pool_id
            )));
        }

        // Export TLS state if this is a TLS connection
        #[cfg(feature = "tls-migration")]
        let tls_state = if let Some(ssl_ptr) = self.ssl_ptr {
            let blob = export_tls_state_from_ptr(ssl_ptr.0)?;
            Some(blob)
        } else {
            None
        };
        #[cfg(not(feature = "tls-migration"))]
        let tls_state: Option<Vec<u8>> = None;

        let state = self.serialize_state(tls_state.is_some())?;

        // SAFETY: raw_fd is a valid open fd stored before tokio::io::split().
        // dup() creates an independent copy; if it fails we return an error.
        let dup_fd = unsafe { libc::dup(raw_fd) };
        if dup_fd < 0 {
            return Err(Error::SocketError(
                "dup() failed during migration".to_string(),
            ));
        }
        Ok(MigrationPayload {
            state,
            fd: dup_fd,
            tls_state,
        })
    }

    fn serialize_state(&self, use_tls: bool) -> Result<BytesMut, Error> {
        let mut buf = BytesMut::with_capacity(512);

        buf.put_u32(MIGRATION_MAGIC);
        buf.put_u16(MIGRATION_VERSION);
        buf.put_u64(self.connection_id);
        buf.put_i32(self.secret_key);
        buf.put_u8(self.transaction_mode as u8);

        put_str(&mut buf, &self.pool_name)?;
        put_str(&mut buf, &self.username)?;

        // Address
        buf.put_u16(self.addr.port());
        let ip_str = self.addr.ip().to_string();
        buf.put_u8(ip_str.len() as u8);
        buf.put_slice(ip_str.as_bytes());

        // Server parameters
        let params = self.server_parameters.as_hashmap();
        serialize_server_parameters(&mut buf, &params)?;

        serialize_prepared_state(&mut buf, &self.prepared)?;

        buf.put_u8(use_tls as u8);

        let pool = self.migration_pool.as_ref().ok_or_else(|| {
            Error::ClientError(format!(
                "migration: no captured pool generation for {}",
                self.cached_pool_id
            ))
        })?;
        if pool.database.is_closed() {
            return Err(Error::ClientError(format!(
                "migration: pool generation {} is closed after reload; reconnect required",
                self.cached_pool_id
            )));
        }

        // Backend auth state (v2): allows new process to skip ScramPending
        // fallback by receiving the ClientKey from the old process.
        if let Some(ref ba_lock) = pool.address.backend_auth {
            match &*ba_lock.read() {
                BackendAuthMethod::Md5PassTheHash(hash) => {
                    buf.put_u8(1);
                    put_str(&mut buf, hash)?;
                }
                BackendAuthMethod::ScramPassthrough(client_key) => {
                    buf.put_u8(2);
                    buf.put_u16(client_key.len() as u16);
                    buf.put_slice(client_key);
                }
                BackendAuthMethod::ScramPending => {
                    buf.put_u8(3);
                }
            }
        } else {
            buf.put_u8(0); // no backend auth
        }

        put_last_anonymous_hash_extension(&mut buf, self.prepared.last_anonymous_hash);
        put_pool_user_extension(&mut buf, &self.cached_pool_id.user)?;
        put_pool_config_hash_extension(&mut buf, Some(pool.config_hash))?;

        Ok(buf)
    }
}

fn serialize_prepared_state(
    buf: &mut BytesMut,
    prepared: &PreparedStatementState,
) -> Result<(), Error> {
    let serializable_entries: Vec<_> = prepared
        .cache
        .iter()
        .filter(|(_, cached)| !cached.intercepted_discard_all)
        .collect();
    let cache_count = serializable_entries.len();
    if cache_count > MAX_PREPARED_ENTRIES {
        return Err(Error::ClientError(format!(
            "migration: cache_count {cache_count} exceeds limit {MAX_PREPARED_ENTRIES}"
        )));
    }
    ensure_migration_state_room(buf, 1 + 1 + 4, "prepared header")?;
    buf.put_u8(prepared.enabled as u8);
    buf.put_u8(prepared.async_client as u8);

    buf.put_u32(cache_count as u32);
    for (key, cached) in serializable_entries {
        let query = cached.parse.query();
        if query.len() > MAX_QUERY_LEN {
            return Err(Error::ClientError(format!(
                "migration: query_len {} exceeds limit {MAX_QUERY_LEN}",
                query.len()
            )));
        }
        let param_types = cached.parse.param_types();
        let param_count = i16::try_from(param_types.len()).map_err(|_| {
            Error::ClientError(format!(
                "migration: param type count {} exceeds i16 frame limit",
                param_types.len()
            ))
        })?;
        let key_len = match key {
            PreparedStatementKeyRef::Named(name) => checked_str_frame_len(name)?,
            PreparedStatementKeyRef::Anonymous(_) => 8,
        };
        let params_len = param_types.len().checked_mul(4).ok_or_else(|| {
            Error::ClientError("migration state length overflow while serializing params".into())
        })?;
        let entry_len = checked_migration_state_sum(
            &[1, key_len, 8, 4, query.len(), 2, params_len],
            "prepared entry",
        )?;
        ensure_migration_state_room(buf, entry_len, "prepared entry")?;

        match key {
            PreparedStatementKeyRef::Named(name) => {
                buf.put_u8(0);
                put_str(buf, name)?;
            }
            PreparedStatementKeyRef::Anonymous(hash) => {
                buf.put_u8(1);
                buf.put_u64(hash);
            }
        }
        buf.put_u64(cached.hash);
        buf.put_u32(query.len() as u32);
        buf.put_slice(query.as_bytes());
        buf.put_i16(param_count);
        for &pt in param_types {
            buf.put_i32(pt);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Deserialization + reconstruction
// ---------------------------------------------------------------------------

struct DeserializedState {
    connection_id: u64,
    secret_key: i32,
    transaction_mode: bool,
    pool_name: String,
    username: String,
    pool_user: String,
    addr: std::net::SocketAddr,
    server_parameters: ServerParameters,
    prepared_enabled: bool,
    async_client: bool,
    prepared_entries: Vec<PreparedEntry>,
    #[allow(dead_code)]
    use_tls: bool,
    backend_auth: Option<BackendAuthMethod>,
    pool_config_hash: Option<u64>,
    last_anonymous_hash: Option<u64>,
}

struct PreparedEntry {
    key: PreparedStatementKey,
    hash: u64,
    query: String,
    param_types: Vec<i32>,
}

fn deserialize_state(mut buf: BytesMut) -> Result<DeserializedState, Error> {
    require(&buf, HEADER_SIZE)?;

    let magic = buf.get_u32();
    if magic != MIGRATION_MAGIC {
        return Err(Error::ClientError(format!(
            "migration: bad magic {magic:#x}"
        )));
    }
    let version = buf.get_u16();
    if version != MIGRATION_VERSION {
        return Err(Error::ClientError(format!(
            "migration: unsupported version {version}"
        )));
    }

    let connection_id = buf.get_u64();
    let secret_key = buf.get_i32();
    let transaction_mode = buf.get_u8() != 0;

    let pool_name = get_str(&mut buf)?;
    let username = get_str(&mut buf)?;

    // Address
    require(&buf, 3)?; // port(2) + ip_len(1)
    let port = buf.get_u16();
    let ip_len = buf.get_u8() as usize;
    require(&buf, ip_len)?;
    let mut ip_bytes = vec![0u8; ip_len];
    buf.copy_to_slice(&mut ip_bytes);
    let ip_str = std::str::from_utf8(&ip_bytes).map_err(|_| Error::ClientError("bad ip".into()))?;
    let ip: std::net::IpAddr = ip_str
        .parse()
        .map_err(|_| Error::ClientError("bad ip parse".into()))?;
    let addr = std::net::SocketAddr::new(ip, port);

    // Server parameters
    require(&buf, 2)?;
    let param_count = buf.get_u16() as usize;
    let mut server_parameters = ServerParameters::new();
    for _ in 0..param_count {
        let k = get_str(&mut buf)?;
        let v = get_str(&mut buf)?;
        server_parameters.set_param(&k, &v, true);
    }

    // Prepared statements
    require(&buf, 2 + 4)?; // enabled(1) + async(1) + count(4)
    let prepared_enabled = buf.get_u8() != 0;
    let async_client = buf.get_u8() != 0;
    let cache_count = buf.get_u32() as usize;
    if cache_count > MAX_PREPARED_ENTRIES {
        return Err(Error::ClientError(format!(
            "migration: cache_count {cache_count} exceeds limit {MAX_PREPARED_ENTRIES}"
        )));
    }
    let mut prepared_entries = Vec::with_capacity(cache_count);
    for _ in 0..cache_count {
        require(&buf, 1)?; // key_type
        let key_type = buf.get_u8();
        let key = match key_type {
            0 => PreparedStatementKey::Named(get_str(&mut buf)?),
            1 => {
                require(&buf, 8)?;
                PreparedStatementKey::Anonymous(buf.get_u64())
            }
            other => {
                return Err(Error::ClientError(format!(
                    "migration: unknown prepared key tag {other}"
                )));
            }
        };
        require(&buf, 8 + 4)?; // hash(8) + query_len(4)
        let hash = buf.get_u64();
        let query_len = buf.get_u32() as usize;
        if query_len > MAX_QUERY_LEN {
            return Err(Error::ClientError(format!(
                "migration: query_len {query_len} exceeds limit {MAX_QUERY_LEN}"
            )));
        }
        require(&buf, query_len)?;
        let mut query_bytes = vec![0u8; query_len];
        buf.copy_to_slice(&mut query_bytes);
        let query = String::from_utf8(query_bytes)
            .map_err(|_| Error::ClientError("bad query utf8".into()))?;
        require(&buf, 2)?; // num_params
        let num_params_raw = buf.get_i16();
        if num_params_raw < 0 {
            return Err(Error::ClientError(format!(
                "migration: negative prepared param count {num_params_raw}"
            )));
        }
        let num_params = num_params_raw as usize;
        let param_bytes = num_params.checked_mul(4).ok_or_else(|| {
            Error::ClientError("migration: prepared param byte count overflow".into())
        })?;
        require(&buf, param_bytes)?;
        let mut param_types = Vec::with_capacity(num_params);
        for _ in 0..num_params {
            param_types.push(buf.get_i32());
        }
        prepared_entries.push(PreparedEntry {
            key,
            hash,
            query,
            param_types,
        });
    }

    require(&buf, 1)?;
    let use_tls = buf.get_u8() != 0;

    // Backend auth state (ScramPassthrough ClientKey, Md5 hash); absent when
    // the migrating client never reached an authenticated backend.
    let backend_auth = if buf.remaining() > 0 {
        let tag = buf.get_u8();
        match tag {
            0 => None,
            1 => {
                // Md5PassTheHash
                let hash = get_str(&mut buf)?;
                Some(BackendAuthMethod::Md5PassTheHash(hash))
            }
            2 => {
                // ScramPassthrough(ClientKey)
                require(&buf, 2)?;
                let key_len = buf.get_u16() as usize;
                require(&buf, key_len)?;
                let mut key = vec![0u8; key_len];
                buf.copy_to_slice(&mut key);
                Some(BackendAuthMethod::ScramPassthrough(key))
            }
            3 => Some(BackendAuthMethod::ScramPending),
            other => {
                return Err(Error::ClientError(format!(
                    "migration: unknown backend auth tag {other}"
                )));
            }
        }
    } else {
        None
    };

    let last_anonymous_hash = get_last_anonymous_hash_extension(&mut buf)?;
    let pool_user = get_pool_user_extension(&mut buf, &username)?;
    let pool_config_hash = get_pool_config_hash_extension(&mut buf)?;

    Ok(DeserializedState {
        connection_id,
        secret_key,
        transaction_mode,
        pool_name,
        username,
        pool_user,
        addr,
        server_parameters,
        prepared_enabled,
        async_client,
        prepared_entries,
        use_tls,
        backend_auth,
        pool_config_hash,
        last_anonymous_hash,
    })
}

fn put_last_anonymous_hash_extension(buf: &mut BytesMut, hash: Option<u64>) {
    buf.put_u8(MIGRATION_EXT_LAST_ANONYMOUS_HASH);
    match hash {
        Some(hash) => {
            buf.put_u8(1);
            buf.put_u64(hash);
        }
        None => buf.put_u8(0),
    }
}

fn get_last_anonymous_hash_extension(buf: &mut BytesMut) -> Result<Option<u64>, Error> {
    if buf.first().copied() != Some(MIGRATION_EXT_LAST_ANONYMOUS_HASH) {
        return Ok(None);
    }

    buf.advance(1);
    require(buf, 1)?;
    match buf.get_u8() {
        0 => Ok(None),
        1 => {
            require(buf, 8)?;
            Ok(Some(buf.get_u64()))
        }
        tag => Err(Error::ClientError(format!(
            "migration: bad last anonymous hash extension tag {tag}"
        ))),
    }
}

fn put_pool_user_extension(buf: &mut BytesMut, pool_user: &str) -> Result<(), Error> {
    let payload_len = checked_str_frame_len(pool_user)?;
    ensure_migration_state_room(buf, 1 + payload_len, "pool user extension")?;
    buf.put_u8(MIGRATION_EXT_POOL_USER);
    put_str(buf, pool_user)
}

fn get_pool_user_extension(buf: &mut BytesMut, fallback: &str) -> Result<String, Error> {
    if buf.first().copied() != Some(MIGRATION_EXT_POOL_USER) {
        return Ok(fallback.to_string());
    }

    buf.advance(1);
    get_str(buf)
}

fn put_pool_config_hash_extension(buf: &mut BytesMut, hash: Option<u64>) -> Result<(), Error> {
    ensure_migration_state_room(
        buf,
        1 + 1 + hash.map(|_| 8).unwrap_or(0),
        "pool config hash",
    )?;
    buf.put_u8(MIGRATION_EXT_POOL_CONFIG_HASH);
    match hash {
        Some(hash) => {
            buf.put_u8(1);
            buf.put_u64(hash);
        }
        None => buf.put_u8(0),
    }
    Ok(())
}

fn get_pool_config_hash_extension(buf: &mut BytesMut) -> Result<Option<u64>, Error> {
    if buf.first().copied() != Some(MIGRATION_EXT_POOL_CONFIG_HASH) {
        return Ok(None);
    }

    buf.advance(1);
    require(buf, 1)?;
    match buf.get_u8() {
        0 => Ok(None),
        1 => {
            require(buf, 8)?;
            Ok(Some(buf.get_u64()))
        }
        tag => Err(Error::ClientError(format!(
            "migration: bad pool config hash extension tag {tag}"
        ))),
    }
}

/// Reconstruct a Client from a migrated fd + serialized state.
pub async fn reconstruct_client(
    fd: RawFd,
    state_buf: BytesMut,
    client_server_map: ClientServerMap,
) -> Result<Client<tokio::io::ReadHalf<TcpStream>, tokio::io::WriteHalf<TcpStream>>, Error> {
    let fd = RawFdGuard::new(fd);
    let state = deserialize_state(state_buf)?;
    if state.use_tls {
        return Err(Error::ClientError(
            "migration: TLS migration state routed to plain TCP importer".into(),
        ));
    }
    ensure_tcp_stream_fd(fd.fd, "plain migration import")?;

    // SAFETY: fd was received via SCM_RIGHTS from the old process and is a valid,
    // open TCP socket. This call takes ownership — no other code holds this fd.
    let std_stream = unsafe { std::net::TcpStream::from_raw_fd(fd.into_raw_fd()) };
    std_stream
        .set_nonblocking(true)
        .map_err(|e| Error::SocketError(format!("set_nonblocking: {e}")))?;
    let stream = TcpStream::from_std(std_stream)
        .map_err(|e| Error::SocketError(format!("from_std: {e}")))?;
    configure_tcp_socket(&stream);

    let raw_fd = Some(stream.as_raw_fd());
    let (read, write) = split(stream);

    let config = get_config();

    // Reconstruct prepared statement cache
    let cached_pool_id = PoolIdentifier::new(&state.pool_name, &state.pool_user);
    let pool = get_pool_by_id(&cached_pool_id);
    ensure_migrated_pool_config_hash_matches(
        pool.as_ref(),
        state.pool_config_hash,
        &state.username,
        &state.pool_name,
    )?;

    restore_backend_auth_if_pending(
        pool.as_ref(),
        state.backend_auth.as_ref(),
        state.pool_config_hash,
        &state.username,
        &state.pool_name,
    );

    let anon_cache_size = resolve_client_anon_cache_size(&state.pool_name, &config.general);
    let planner_param_hash = state.server_parameters.planner_param_hash();

    let prepared = reconstruct_prepared_state(
        state.prepared_enabled,
        state.async_client,
        &state.prepared_entries,
        state.last_anonymous_hash,
        pool.as_ref(),
        anon_cache_size,
        planner_param_hash,
    );

    let application_name = state
        .server_parameters
        .as_hashmap()
        .get("application_name")
        .cloned()
        .unwrap_or_default();

    let stats = Arc::new(ClientStats::new_with_pool_user(
        state.connection_id,
        &application_name,
        &state.username,
        &state.pool_name,
        &state.pool_user,
        &state.addr.to_string(),
        crate::utils::clock::now(),
        false, // plain TCP
    ));

    // rebuild cached PoolIdentifier on the migrated side.
    Ok(Client {
        // 64 KiB BufReader on migration import - matches
        // startup path so reconstructed clients keep the same buffer
        // capacity.
        read: BufReader::with_capacity(crate::server::BUF_STREAM_CAPACITY, read),
        write,
        buffer: PooledBuffer::new(),
        addr: state.addr,
        addr_str: state.addr.to_string(),
        read_buf: BytesMut::with_capacity(8192),
        connection_id: state.connection_id,
        cancel_mode: false,
        transaction_mode: state.transaction_mode,
        sql_prepare_session_pinned: false,
        secret_key: state.secret_key,
        client_server_map,
        stats,
        admin: false,
        last_server_stats: None,
        connected_to_server: false,
        session_xact_start: None,
        pool_name: state.pool_name,
        username: state.username,
        cached_pool_id,
        migration_pool: None,
        migration_pool_is_dynamic: false,
        server_parameters: state.server_parameters,
        prepared,
        client_last_messages_in_tx: PooledBuffer::new(),
        max_memory_usage: config.general.max_memory_usage.as_bytes(),
        client_pending_begin: None,
        pending_app_name_set: None,
        #[cfg(unix)]
        raw_fd,
        #[cfg(all(unix, feature = "tls-migration"))]
        ssl_ptr: None,
    })
}

/// Reconstruct a TLS Client from a migrated fd + serialized state + TLS blob.
#[cfg(all(target_os = "linux", feature = "tls-migration"))]
pub async fn reconstruct_tls_client(
    fd: RawFd,
    state_buf: BytesMut,
    client_server_map: ClientServerMap,
    tls_blob: &[u8],
    tls_acceptor: Option<tokio_native_tls::TlsAcceptor>,
) -> Result<
    Client<
        tokio::io::ReadHalf<tokio_native_tls::TlsStream<TcpStream>>,
        tokio::io::WriteHalf<tokio_native_tls::TlsStream<TcpStream>>,
    >,
    Error,
> {
    let fd = RawFdGuard::new(fd);
    let state = deserialize_state(state_buf)?;
    if !state.use_tls {
        return Err(Error::ClientError(
            "migration: plain TCP migration state routed to TLS importer".into(),
        ));
    }
    let acceptor = tls_acceptor.ok_or_else(|| Error::ClientError("no TLS acceptor".into()))?;
    ensure_tcp_stream_fd(fd.fd, "TLS migration import")?;

    // SAFETY: fd was received via SCM_RIGHTS and is a valid TCP socket.
    let std_stream = unsafe { std::net::TcpStream::from_raw_fd(fd.into_raw_fd()) };
    std_stream
        .set_nonblocking(true)
        .map_err(|e| Error::SocketError(format!("set_nonblocking: {e}")))?;
    let tcp_stream = TcpStream::from_std(std_stream)
        .map_err(|e| Error::SocketError(format!("from_std: {e}")))?;
    configure_tcp_socket(&tcp_stream);

    let tls_fd = tcp_stream.as_raw_fd();
    let raw_fd = Some(tls_fd);

    let tls_stream = acceptor
        .import_migration_state(tcp_stream, tls_blob, tls_fd)
        .map_err(|e| Error::ClientError(format!("TLS import failed: {e}")))?;

    let ssl_ptr = Some(crate::client::core::SslRawPtr(
        tls_stream.get_ref().ssl_raw_ptr(),
    ));
    let (read, write) = split(tls_stream);

    let config = get_config();
    let cached_pool_id = PoolIdentifier::new(&state.pool_name, &state.pool_user);
    let pool = get_pool_by_id(&cached_pool_id);
    ensure_migrated_pool_config_hash_matches(
        pool.as_ref(),
        state.pool_config_hash,
        &state.username,
        &state.pool_name,
    )?;

    restore_backend_auth_if_pending(
        pool.as_ref(),
        state.backend_auth.as_ref(),
        state.pool_config_hash,
        &state.username,
        &state.pool_name,
    );

    let anon_cache_size = resolve_client_anon_cache_size(&state.pool_name, &config.general);
    let planner_param_hash = state.server_parameters.planner_param_hash();

    let prepared = reconstruct_prepared_state(
        state.prepared_enabled,
        state.async_client,
        &state.prepared_entries,
        state.last_anonymous_hash,
        pool.as_ref(),
        anon_cache_size,
        planner_param_hash,
    );

    let application_name = state
        .server_parameters
        .as_hashmap()
        .get("application_name")
        .cloned()
        .unwrap_or_default();

    let stats = Arc::new(ClientStats::new_with_pool_user(
        state.connection_id,
        &application_name,
        &state.username,
        &state.pool_name,
        &state.pool_user,
        &state.addr.to_string(),
        crate::utils::clock::now(),
        true, // TLS
    ));

    // rebuild cached PoolIdentifier on the migrated side.
    Ok(Client {
        // 64 KiB BufReader on migration import - matches
        // startup path so reconstructed clients keep the same buffer
        // capacity.
        read: BufReader::with_capacity(crate::server::BUF_STREAM_CAPACITY, read),
        write,
        buffer: PooledBuffer::new(),
        addr: state.addr,
        addr_str: state.addr.to_string(),
        read_buf: BytesMut::with_capacity(8192),
        connection_id: state.connection_id,
        cancel_mode: false,
        transaction_mode: state.transaction_mode,
        sql_prepare_session_pinned: false,
        secret_key: state.secret_key,
        client_server_map,
        stats,
        admin: false,
        last_server_stats: None,
        connected_to_server: false,
        session_xact_start: None,
        pool_name: state.pool_name,
        username: state.username,
        cached_pool_id,
        migration_pool: None,
        migration_pool_is_dynamic: false,
        server_parameters: state.server_parameters,
        prepared,
        client_last_messages_in_tx: PooledBuffer::new(),
        max_memory_usage: config.general.max_memory_usage.as_bytes(),
        client_pending_begin: None,
        pending_app_name_set: None,
        #[cfg(unix)]
        raw_fd,
        #[cfg(all(unix, feature = "tls-migration"))]
        ssl_ptr,
    })
}

fn reconstruct_prepared_state(
    enabled: bool,
    async_client: bool,
    entries: &[PreparedEntry],
    last_anonymous_hash: Option<u64>,
    pool: Option<&ConnectionPool>,
    cache_size: usize,
    planner_param_hash: u64,
) -> PreparedStatementState {
    let mut prepared = PreparedStatementState::new(enabled, cache_size);
    prepared.async_client = async_client;

    let Some(pool) = pool else {
        return prepared;
    };
    for entry in entries {
        let parse = Parse::from_parts(&entry.query, &entry.param_types);
        let hash = entry.hash;
        // Forward the client-given name from the deserialised blob so that the
        // pool cache's seen_as_named / seen_as_anonymous flags survive the
        // binary upgrade. Anonymous keys carry no name; pass `None`.
        let client_given_name: Option<&str> = match &entry.key {
            PreparedStatementKey::Named(name) => Some(name.as_str()),
            PreparedStatementKey::Anonymous(_) => None,
        };
        let Some(shared_parse) =
            pool.register_parse_to_cache(hash, &parse, client_given_name, planner_param_hash)
        else {
            continue;
        };
        // `Arc<str>` instead of `String` to match the migrated
        // `CachedStatement.async_name` type.
        let async_name: Option<std::sync::Arc<str>> = if async_client {
            Some(std::sync::Arc::<str>::from(
                format!(
                    "DOORMAN_async_{}",
                    PREPARED_STATEMENT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                )
                .as_str(),
            ))
        } else {
            None
        };
        let cached = CachedStatement {
            set_cleanup_command: extract_set_cleanup_commands(shared_parse.query().as_bytes())
                .first()
                .copied(),
            reset_cleanup_command: extract_reset_cleanup_commands(shared_parse.query().as_bytes())
                .first()
                .copied(),
            parse: shared_parse,
            hash,
            intercepted_discard_all: false,
            async_name,
        };
        // Replay-evictions during reconstruction are an artefact of the new
        // LRU cap vs the size of the migration blob, not real workload
        // pressure on the running pooler. Drop the outcome instead of
        // bumping per-client and Prometheus counters; the operator only
        // wants to see evictions caused by live traffic.
        let _ = prepared.cache.put(entry.key.clone(), cached);
    }
    if let Some(hash) = last_anonymous_hash {
        if prepared
            .cache
            .get(&PreparedStatementKey::Anonymous(hash))
            .is_some()
        {
            prepared.last_anonymous_hash = Some(hash);
        }
    } else {
        // Legacy v2 senders did not carry the lookup pointer. Recover the
        // common unambiguous case without guessing when multiple anonymous
        // entries survived migration.
        let mut anonymous_hashes = prepared.cache.iter().filter_map(|(key, _)| match key {
            PreparedStatementKeyRef::Anonymous(hash) => Some(hash),
            PreparedStatementKeyRef::Named(_) => None,
        });
        if let Some(hash) = anonymous_hashes.next() {
            if anonymous_hashes.next().is_none() {
                prepared.last_anonymous_hash = Some(hash);
            }
        }
    }
    prepared
}

// ---------------------------------------------------------------------------
// SCM_RIGHTS fd passing
// ---------------------------------------------------------------------------

/// Send a migration payload (fd + state) over a Unix socket.
/// After successful send, the fd in payload is set to -1 to prevent double-close.
pub fn send_migration_fd(socket_fd: RawFd, payload: &mut MigrationPayload) -> Result<(), Error> {
    let tls_data = payload.tls_state.as_deref().unwrap_or(&[]);
    let state_len =
        checked_migration_frame_len("state", payload.state.len(), MAX_MIGRATION_STATE_LEN)?;
    let tls_len = checked_migration_frame_len("tls", tls_data.len(), MAX_MIGRATION_TLS_STATE_LEN)?;
    let frame_len = 4usize
        .checked_add(payload.state.len())
        .and_then(|len| len.checked_add(4))
        .and_then(|len| len.checked_add(tls_data.len()))
        .ok_or_else(|| Error::SocketError("migration frame length overflow".to_string()))?;
    let mut msg_buf = Vec::with_capacity(frame_len);
    msg_buf.extend_from_slice(&state_len.to_be_bytes());
    msg_buf.extend_from_slice(&payload.state);
    msg_buf.extend_from_slice(&tls_len.to_be_bytes());
    msg_buf.extend_from_slice(tls_data);

    let iov = libc::iovec {
        iov_base: msg_buf.as_ptr() as *mut libc::c_void,
        iov_len: msg_buf.len(),
    };

    let fd_to_send = payload.fd;
    // SAFETY: CMSG_SPACE returns the correct buffer size for one RawFd.
    let cmsg_space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];

    // SAFETY: zeroed msghdr is a valid initial state for sendmsg.
    let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
    msghdr.msg_iov = &iov as *const _ as *mut _;
    msghdr.msg_iovlen = 1;
    msghdr.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msghdr.msg_controllen = cmsg_space as _;

    // SAFETY: cmsg_buf is correctly sized via CMSG_SPACE. CMSG_FIRSTHDR, CMSG_LEN,
    // CMSG_DATA return valid pointers into cmsg_buf. fd_to_send is a valid open fd.
    // sendmsg is called with a valid msghdr pointing to valid iov and cmsg buffers.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msghdr);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::copy_nonoverlapping(
            &fd_to_send as *const RawFd as *const u8,
            libc::CMSG_DATA(cmsg),
            std::mem::size_of::<RawFd>(),
        );

        let ret = libc::sendmsg(socket_fd, &msghdr, 0);
        if ret < 0 {
            return Err(Error::SocketError(format!(
                "sendmsg: {}",
                std::io::Error::last_os_error()
            )));
        }
        // on AF_UNIX the SCM_RIGHTS fd is
        // delivered to the receiver together with the FIRST data byte, so a
        // short sendmsg (ret < msg_buf.len()) has ALREADY transferred the fd
        // while leaving the data frame truncated. The old code returned Err
        // here, leaving the receiver holding a live client fd plus an
        // unparseable frame - a half-migrated client. Complete the frame with
        // plain send() (the fd rides the sendmsg above and is sent exactly
        // once) so the receiver reconstructs the full state and no fd leaks.
        let mut sent = ret as usize;
        while sent < msg_buf.len() {
            let chunk = &msg_buf[sent..];
            let n = libc::send(
                socket_fd,
                chunk.as_ptr() as *const libc::c_void,
                chunk.len(),
                0,
            );
            if n < 0 {
                let err = std::io::Error::last_os_error();
                match err.kind() {
                    std::io::ErrorKind::Interrupted => continue,
                    std::io::ErrorKind::WouldBlock => {
                        // Non-blocking socket whose send buffer is full: wait
                        // for it to drain instead of failing a frame whose fd
                        // is already in the receiver's table.
                        let mut pfd = libc::pollfd {
                            fd: socket_fd,
                            events: libc::POLLOUT,
                            revents: 0,
                        };
                        libc::poll(&mut pfd, 1, -1);
                        continue;
                    }
                    _ => {
                        return Err(Error::SocketError(format!(
                            "send completing migration frame at {sent}/{}: {err}",
                            msg_buf.len()
                        )));
                    }
                }
            }
            if n == 0 {
                return Err(Error::SocketError(format!(
                    "peer closed completing migration frame ({sent}/{} bytes)",
                    msg_buf.len()
                )));
            }
            sent += n as usize;
        }
    }

    // SAFETY: sendmsg duplicated the fd into the receiver's fd table.
    // We close our copy to avoid a leak. Setting fd = -1 prevents Drop from
    // closing it again.
    unsafe { libc::close(payload.fd) };
    payload.fd = -1;
    Ok(())
}

fn checked_migration_frame_len(label: &str, len: usize, max: usize) -> Result<u32, Error> {
    if len > max {
        return Err(Error::SocketError(format!(
            "migration: {label} length {len} exceeds limit {max}"
        )));
    }
    u32::try_from(len).map_err(|_| {
        Error::SocketError(format!(
            "migration: {label} length {len} exceeds u32 frame limit"
        ))
    })
}

/// Receive a migration payload (fd + state + optional TLS state) from a Unix socket.
/// Returns (raw_fd, state_bytes, tls_state) or error on EOF/failure.
pub fn recv_migration_fd(socket_fd: RawFd) -> Result<(RawFd, BytesMut, Option<Vec<u8>>), Error> {
    let mut recv_buf = vec![0u8; MAX_RECV_BUF];
    let iov = libc::iovec {
        iov_base: recv_buf.as_mut_ptr() as *mut libc::c_void,
        iov_len: recv_buf.len(),
    };

    // SAFETY: CMSG_SPACE returns the correct buffer size for one RawFd.
    let cmsg_space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];

    // SAFETY: zeroed msghdr is a valid initial state for recvmsg.
    let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
    msghdr.msg_iov = &iov as *const _ as *mut _;
    msghdr.msg_iovlen = 1;
    msghdr.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
    msghdr.msg_controllen = cmsg_space as _;

    let n = recvmsg_retrying_interrupted(socket_fd, &mut msghdr)?;
    if n == 0 {
        return Err(Error::SocketError("migration socket closed".to_string()));
    }
    let n = n as usize;

    // Extract fd from cmsg
    let mut received_fd: RawFd = -1;
    // SAFETY: CMSG_FIRSTHDR and CMSG_NXTHDR return valid pointers into the
    // cmsg_buf that was filled by recvmsg, or null when exhausted.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msghdr);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                std::ptr::copy_nonoverlapping(
                    libc::CMSG_DATA(cmsg),
                    &mut received_fd as *mut RawFd as *mut u8,
                    std::mem::size_of::<RawFd>(),
                );
            }
            cmsg = libc::CMSG_NXTHDR(&msghdr, cmsg);
        }
    }

    if received_fd < 0 {
        return Err(Error::SocketError("migration: no fd in cmsg".to_string()));
    }
    if let Err(err) = set_close_on_exec(received_fd) {
        close_raw_fd(received_fd);
        return Err(err);
    }

    if n < 4 {
        close_raw_fd(received_fd);
        return Err(Error::SocketError(
            "migration: message too short for length prefix".to_string(),
        ));
    }

    let state_len =
        u32::from_be_bytes([recv_buf[0], recv_buf[1], recv_buf[2], recv_buf[3]]) as usize;
    if state_len > MAX_MIGRATION_STATE_LEN {
        close_raw_fd(received_fd);
        return Err(Error::SocketError(format!(
            "migration: state length {state_len} exceeds limit {MAX_MIGRATION_STATE_LEN}"
        )));
    }
    let data_received = n - 4;

    let mut state = BytesMut::with_capacity(state_len);
    if data_received <= state_len {
        state.put_slice(&recv_buf[4..4 + data_received]);
    } else {
        state.put_slice(&recv_buf[4..4 + state_len]);
    }

    // If we didn't get all data in one recvmsg, read the rest
    while state.len() < state_len {
        let remaining = state_len - state.len();
        let chunk_size = remaining.min(recv_buf.len());
        let n =
            match recv_retrying_interrupted(socket_fd, &mut recv_buf[..chunk_size], "state read") {
                Ok(n) => n,
                Err(err) => {
                    close_raw_fd(received_fd);
                    return Err(err);
                }
            };
        if n == 0 {
            close_raw_fd(received_fd);
            return Err(Error::SocketError("migration: truncated state".into()));
        }
        state.put_slice(&recv_buf[..n]);
    }

    // Read TLS state length + data (follows the app state)
    // May need to read more data from socket if not all arrived in first recvmsg
    let mut tls_header = [0u8; 4];
    let mut tls_header_read = 0usize;

    // Check if tls_len header is already in our buffer
    let leftover = data_received.saturating_sub(state_len);
    if leftover >= 4 {
        // TLS length header is in the buffer
        let off = 4 + state_len;
        tls_header.copy_from_slice(&recv_buf[off..off + 4]);
        tls_header_read = 4;
    } else if leftover > 0 {
        tls_header[..leftover].copy_from_slice(&recv_buf[4 + state_len..4 + state_len + leftover]);
        tls_header_read = leftover;
    }

    // Read remaining TLS header bytes if needed
    while tls_header_read < 4 {
        let n = match recv_retrying_interrupted(
            socket_fd,
            &mut recv_buf[..4 - tls_header_read],
            "tls header read",
        ) {
            Ok(n) => n,
            Err(err) => {
                close_raw_fd(received_fd);
                return Err(err);
            }
        };
        if n == 0 {
            close_raw_fd(received_fd);
            return Err(Error::SocketError("migration: truncated tls header".into()));
        }
        tls_header[tls_header_read..tls_header_read + n].copy_from_slice(&recv_buf[..n]);
        tls_header_read += n;
    }

    let tls_len = u32::from_be_bytes(tls_header) as usize;
    if tls_len > MAX_MIGRATION_TLS_STATE_LEN {
        close_raw_fd(received_fd);
        return Err(Error::SocketError(format!(
            "migration: tls length {tls_len} exceeds limit {MAX_MIGRATION_TLS_STATE_LEN}"
        )));
    }
    let tls_state = if tls_len > 0 {
        let mut tls_buf = Vec::new();
        if let Err(err) = tls_buf.try_reserve_exact(tls_len) {
            close_raw_fd(received_fd);
            return Err(Error::SocketError(format!(
                "migration: could not reserve tls length {tls_len}: {err}"
            )));
        }
        tls_buf.resize(tls_len, 0);
        // Check if some TLS data was already in the original recv buffer
        let tls_data_offset = 4 + state_len + 4;
        let mut tls_read = if n > tls_data_offset {
            let avail = (n - tls_data_offset).min(tls_len);
            tls_buf[..avail].copy_from_slice(&recv_buf[tls_data_offset..tls_data_offset + avail]);
            avail
        } else {
            0
        };

        while tls_read < tls_len {
            let remaining = tls_len - tls_read;
            let chunk = remaining.min(recv_buf.len());
            let nr = match recv_retrying_interrupted(
                socket_fd,
                &mut recv_buf[..chunk],
                "tls state read",
            ) {
                Ok(n) => n,
                Err(err) => {
                    close_raw_fd(received_fd);
                    return Err(err);
                }
            };
            if nr == 0 {
                close_raw_fd(received_fd);
                return Err(Error::SocketError("migration: truncated tls state".into()));
            }
            tls_buf[tls_read..tls_read + nr].copy_from_slice(&recv_buf[..nr]);
            tls_read += nr;
        }
        Some(tls_buf)
    } else {
        None
    };

    Ok((received_fd, state, tls_state))
}

// ---------------------------------------------------------------------------
// Sender / receiver tasks
// ---------------------------------------------------------------------------

/// Sender task: runs in the OLD process.
/// Reads MigrationPayload from channel, sends over Unix socket.
/// Accepts a shutdown receiver: when the main loop exits, it drops the
/// sender half, which makes shutdown_rx.recv() return None, breaking
/// the loop. Without this the task blocks forever on rx.recv() because
/// MIGRATION_TX lives in a static OnceLock and is never dropped.
pub async fn migration_sender_task(
    socket_fd: RawFd,
    mut rx: mpsc::Receiver<MigrationPayload>,
    mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
) {
    async fn send_payload(socket_fd: RawFd, mut payload: MigrationPayload) {
        let result =
            tokio::task::spawn_blocking(move || send_migration_fd(socket_fd, &mut payload)).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("migration send failed: {e}"),
            Err(e) if e.is_panic() => error!("migration send task panicked: {e}"),
            Err(e) => warn!("migration send task join error: {e:?}"),
        }
    }

    loop {
        tokio::select! {
            payload = rx.recv() => {
                match payload {
                    Some(p) => send_payload(socket_fd, p).await,
                    None => break,
                }
            }
            _ = &mut shutdown_rx => {
                info!("migration sender: shutdown signal received");
                rx.close();
                while let Some(p) = rx.recv().await {
                    send_payload(socket_fd, p).await;
                }
                break;
            }
        }
    }
    info!("migration sender: closing socket");
    // SAFETY: socket_fd is the parent end of the socketpair, owned by this task.
    unsafe { libc::close(socket_fd) };
}

/// Receiver task: runs in the NEW process.
/// Reads migrated clients from Unix socket, reconstructs and spawns them.
fn try_acquire_migrated_client_count_guard(
    max_connections: u64,
) -> Option<crate::app::server::ClientCountGuard> {
    let guard = crate::app::server::ClientCountGuard::acquire();
    let current_clients =
        crate::app::server::CURRENT_CLIENT_COUNT.load(std::sync::atomic::Ordering::SeqCst);
    if current_clients as u64 > max_connections {
        drop(guard);
        None
    } else {
        Some(guard)
    }
}

fn try_acquire_migrated_client_count_guard_or_close_fd(
    max_connections: u64,
    fd: RawFd,
) -> Option<crate::app::server::ClientCountGuard> {
    match try_acquire_migrated_client_count_guard(max_connections) {
        Some(guard) => Some(guard),
        None => {
            close_raw_fd(fd);
            None
        }
    }
}

pub async fn migration_receiver_task(
    socket_fd: RawFd,
    client_server_map: ClientServerMap,
    _tls_acceptor: Option<tokio_native_tls::TlsAcceptor>,
) {
    #[cfg(all(target_os = "linux", feature = "tls-migration"))]
    let tls_acceptor = _tls_acceptor;
    use crate::stats::TOTAL_CONNECTION_COUNTER;
    use std::sync::atomic::Ordering;

    info!("migration receiver: listening for migrated clients");

    loop {
        let result = tokio::task::spawn_blocking(move || recv_migration_fd(socket_fd)).await;

        match result {
            Ok(Ok((fd, state_buf, tls_state))) => {
                if let Some(_tls_blob) = tls_state {
                    #[cfg(all(target_os = "linux", feature = "tls-migration"))]
                    {
                        let max_connections = get_config().general.max_connections;
                        let Some(client_count_guard) =
                            try_acquire_migrated_client_count_guard_or_close_fd(
                                max_connections,
                                fd,
                            )
                        else {
                            warn!(
                                "migrated TLS client fd rejected before reconstruction: too many clients (max={})",
                                max_connections,
                            );
                            continue;
                        };
                        let csm = client_server_map.clone();
                        let acceptor = tls_acceptor.clone();
                        let tls_blob = _tls_blob;
                        tokio::spawn(async move {
                            let _client_count_guard = client_count_guard;
                            match reconstruct_tls_client(fd, state_buf, csm, &tls_blob, acceptor)
                                .await
                            {
                                Ok(mut client) => {
                                    TOTAL_CONNECTION_COUNTER.fetch_max(
                                        client.connection_id as usize,
                                        Ordering::Relaxed,
                                    );
                                    info!(
                                        "[{}@{} #c{}] migrated TLS client from {}",
                                        client.username,
                                        client.pool_name,
                                        client.connection_id,
                                        client.addr
                                    );
                                    let result = client.handle().await;
                                    if !client.is_admin() && result.is_err() {
                                        client.disconnect_stats();
                                    }
                                }
                                Err(e) => {
                                    error!("failed to reconstruct migrated TLS client: {e}");
                                }
                            }
                        });
                    }
                    #[cfg(not(all(target_os = "linux", feature = "tls-migration")))]
                    {
                        warn!("TLS migration not available; closing fd");
                        let _ = (state_buf, _tls_blob);
                        unsafe { libc::close(fd) };
                    }
                    continue;
                }
                let max_connections = get_config().general.max_connections;
                let Some(client_count_guard) =
                    try_acquire_migrated_client_count_guard_or_close_fd(max_connections, fd)
                else {
                    warn!(
                        "migrated client fd rejected before reconstruction: too many clients (max={max_connections})",
                    );
                    continue;
                };
                let csm = client_server_map.clone();
                tokio::spawn(async move {
                    let _client_count_guard = client_count_guard;
                    match reconstruct_client(fd, state_buf, csm).await {
                        Ok(mut client) => {
                            // Advance the global counter past the migrated id so new
                            // connections don't collide with migrated client ids.
                            TOTAL_CONNECTION_COUNTER
                                .fetch_max(client.connection_id as usize, Ordering::Relaxed);
                            info!(
                                "[{}@{} #c{}] migrated client accepted from {}",
                                client.username,
                                client.pool_name,
                                client.connection_id,
                                client.addr
                            );
                            let result = client.handle().await;
                            if !client.is_admin() && result.is_err() {
                                warn!(
                                    "[{}@{} #c{}] migrated client {} error: {}",
                                    client.username,
                                    client.pool_name,
                                    client.connection_id,
                                    client.addr,
                                    result.as_ref().unwrap_err()
                                );
                                client.disconnect_stats();
                            }
                        }
                        Err(e) => {
                            error!("failed to reconstruct migrated client: {e}");
                        }
                    }
                });
            }
            Ok(Err(e)) => {
                match migration_receive_error_kind(&e) {
                    MigrationReceiveErrorKind::Eof => {
                        info!("migration receiver done: {e}");
                    }
                    MigrationReceiveErrorKind::Failure(reason) => {
                        crate::web::metrics::record_migration_receiver_failure(reason);
                        warn!("migration receiver failed while receiving migrated client: {e}");
                    }
                }
                break;
            }
            Err(e) => {
                let reason = if e.is_panic() {
                    "join_panic"
                } else {
                    "join_error"
                };
                crate::web::metrics::record_migration_receiver_failure(reason);
                error!("migration receiver join failure: {e}");
                break;
            }
        }
    }

    // SAFETY: socket_fd is the child end of the socketpair, owned by this task.
    unsafe { libc::close(socket_fd) };
    info!("migration receiver: stopped");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::{register_dynamic_pool, PoolIdentifier, DYNAMIC_POOLS};
    use dashmap::DashMap;
    use std::fs::File;
    use std::sync::Arc;
    use tokio::io::{empty, sink, BufReader, Empty, Sink};

    struct DynamicPoolMembershipGuard(PoolIdentifier);

    impl DynamicPoolMembershipGuard {
        fn register(id: PoolIdentifier) -> Self {
            register_dynamic_pool(&id);
            Self(id)
        }
    }

    impl Drop for DynamicPoolMembershipGuard {
        fn drop(&mut self) {
            let current = DYNAMIC_POOLS.load();
            let mut next = (**current).clone();
            next.remove(&self.0);
            DYNAMIC_POOLS.store(Arc::new(next));
        }
    }

    fn migration_test_client(use_tls: bool) -> Client<Empty, Sink> {
        let addr = "127.0.0.1:6543".parse().unwrap();
        Client {
            read: BufReader::new(empty()),
            write: sink(),
            buffer: PooledBuffer::new(),
            addr,
            addr_str: addr.to_string(),
            read_buf: BytesMut::new(),
            connection_id: 1,
            cancel_mode: false,
            transaction_mode: false,
            sql_prepare_session_pinned: false,
            secret_key: 0,
            client_server_map: Arc::new(DashMap::new()),
            stats: Arc::new(ClientStats::new(
                1,
                "test_app",
                "user",
                "db",
                "127.0.0.1:6543",
                crate::utils::clock::now(),
                use_tls,
            )),
            admin: false,
            last_server_stats: None,
            connected_to_server: false,
            session_xact_start: None,
            pool_name: "db".to_string(),
            username: "user".to_string(),
            cached_pool_id: crate::pool::PoolIdentifier::new("db", "user"),
            migration_pool: Some(ConnectionPool::test_for_protocol()),
            migration_pool_is_dynamic: false,
            server_parameters: ServerParameters::default(),
            prepared: PreparedStatementState::new(true, 0),
            max_memory_usage: u64::MAX,
            client_last_messages_in_tx: PooledBuffer::new(),
            client_pending_begin: None,
            pending_app_name_set: None,
            #[cfg(unix)]
            raw_fd: None,
            #[cfg(all(unix, feature = "tls-migration"))]
            ssl_ptr: None,
        }
    }

    #[test]
    #[serial_test::serial(migration_dynamic_pool)]
    fn prepare_migration_rejects_dynamic_pool_clients_before_fd_dup() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let client_stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_server_stream, _) = listener.accept().unwrap();

        let mut client = migration_test_client(false);
        let _dynamic_guard = DynamicPoolMembershipGuard::register(client.cached_pool_id.clone());
        client.migration_pool_is_dynamic = true;
        client.raw_fd = Some(client_stream.as_raw_fd());

        let err = match client.prepare_migration() {
            Ok(payload) => {
                close_raw_fd(payload.fd);
                panic!("dynamic auth_query clients must fail closed before fd handoff");
            }
            Err(err) => err,
        };
        assert!(
            matches!(err, Error::ClientError(ref msg) if msg.contains("dynamic auth_query pool")),
            "unexpected dynamic migration error: {err:?}"
        );
    }

    #[test]
    fn backend_auth_restore_requires_pool_config_hash_match() {
        let src = include_str!("migration.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);

        let restore_start = impl_src
            .find("fn restore_backend_auth_if_pending(")
            .expect("backend auth restore helper should exist");
        let restore_body = &impl_src[restore_start..];
        let restore_end = restore_body
            .find("\n}\n\nconst MIGRATION_MAGIC")
            .expect("restore helper should precede migration constants");
        let restore_body = &restore_body[..restore_end];

        assert!(
            restore_body.contains("migrated_pool_config_hash"),
            "migrated backend auth restore must receive the sender pool config identity"
        );
        assert!(
            restore_body.contains("pool.config_hash != migrated_pool_config_hash"),
            "migrated backend auth must not overwrite a same-id pool after config/password rotation"
        );

        assert!(
            impl_src.contains("MIGRATION_EXT_POOL_CONFIG_HASH"),
            "migration state must serialize the sender pool config identity"
        );
    }

    #[test]
    fn backend_auth_restore_skips_mismatched_pool_config_hash() {
        let mut pool = ConnectionPool::test_for_protocol();
        pool.config_hash = 0x2222;
        pool.address.backend_auth = Some(Arc::new(parking_lot::RwLock::new(
            BackendAuthMethod::ScramPending,
        )));

        let migrated_auth = BackendAuthMethod::ScramPassthrough(vec![1, 2, 3]);
        restore_backend_auth_if_pending(
            Some(&pool),
            Some(&migrated_auth),
            Some(0x1111),
            "user",
            "db",
        );

        let backend_auth = pool.address.backend_auth.as_ref().unwrap().read();
        assert!(
            matches!(*backend_auth, BackendAuthMethod::ScramPending),
            "mismatched migrated pool config hash must not overwrite ScramPending"
        );
    }

    #[test]
    fn migrated_client_reconstruction_requires_pool_config_hash_match() {
        let src = include_str!("migration.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);

        for function_name in [
            "pub async fn reconstruct_client(",
            "pub async fn reconstruct_tls_client(",
        ] {
            let Some(start) = impl_src.find(function_name) else {
                continue;
            };
            let body = &impl_src[start..];
            let end = body
                .find("\nfn reconstruct_prepared_state")
                .unwrap_or(body.len());
            let body = &body[..end];

            let pool_lookup = body
                .find("let pool = get_pool_by_id(&cached_pool_id);")
                .unwrap_or_else(|| panic!("{function_name} must look up the migrated pool"));
            let hash_check = body
                .find("ensure_migrated_pool_config_hash_matches(")
                .unwrap_or_else(|| {
                    panic!("{function_name} must reject stale migrated pool config hashes")
                });
            let restore = body
                .find("restore_backend_auth_if_pending(")
                .unwrap_or_else(|| panic!("{function_name} must restore backend auth"));
            let prepared = body
                .find("reconstruct_prepared_state(")
                .unwrap_or_else(|| panic!("{function_name} must reconstruct prepared state"));

            assert!(
                pool_lookup < hash_check && hash_check < restore && hash_check < prepared,
                "{function_name} must reject stale migrated pool config hashes before \
                 backend auth or prepared state is restored"
            );
        }
    }

    #[test]
    fn migration_sender_serializes_held_pool_generation() {
        let src = include_str!("migration.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);

        let prepare_start = impl_src
            .find("pub fn prepare_migration(&self)")
            .expect("prepare_migration should exist");
        let prepare_body = &impl_src[prepare_start..];
        let prepare_end = prepare_body
            .find("\n    fn serialize_state")
            .expect("serialize_state should follow prepare_migration");
        let prepare_body = &prepare_body[..prepare_end];
        assert!(
            prepare_body.contains("self.migration_pool_is_dynamic"),
            "migration eligibility must use the pool generation captured at auth/handle time"
        );
        assert!(
            !prepare_body.contains("is_dynamic_pool(&self.cached_pool_id)"),
            "migration eligibility must not re-read dynamic membership from live global maps"
        );

        let serialize_start = impl_src
            .find("fn serialize_state(&self")
            .expect("serialize_state should exist");
        let serialize_body = &impl_src[serialize_start..];
        let serialize_end = serialize_body
            .find("\n}\n\nfn serialize_prepared_state")
            .expect("serialize_state should precede prepared-state serializer");
        let serialize_body = &serialize_body[..serialize_end];
        assert!(
            serialize_body.contains("self.migration_pool.as_ref()"),
            "migration state must serialize config hash/backend auth from the held pool generation"
        );
        assert!(
            !serialize_body.contains("get_pool_by_id(&self.cached_pool_id)"),
            "migration state must not sample config hash/backend auth from live global POOLS"
        );
        assert!(
            serialize_body.contains("pool.database.is_closed()"),
            "closed/replaced held pool generations must fail closed before fd migration"
        );
    }

    #[test]
    fn put_str_get_str_roundtrip() {
        let mut buf = BytesMut::new();
        put_str(&mut buf, "hello").unwrap();
        put_str(&mut buf, "").unwrap();
        put_str(&mut buf, "мир").unwrap(); // multibyte utf-8

        let mut cur = buf.freeze();
        assert_eq!(get_str(&mut cur).unwrap(), "hello");
        assert_eq!(get_str(&mut cur).unwrap(), "");
        assert_eq!(get_str(&mut cur).unwrap(), "мир");
        assert_eq!(cur.remaining(), 0);
    }

    #[test]
    fn put_str_rejects_oversized_string_before_writing_payload() {
        let oversized = "x".repeat(u16::MAX as usize + 1);
        let mut buf = BytesMut::new();

        let err = put_str(&mut buf, &oversized).unwrap_err();

        assert!(err.to_string().contains("exceeds u16 frame limit"));
        assert!(
            buf.is_empty(),
            "failed string encoding must not leave a partial migration frame"
        );
    }

    #[test]
    fn get_str_truncated() {
        let mut buf = BytesMut::new();
        buf.put_u16(100); // claims 100 bytes but has none
        let mut cur = buf.freeze();
        assert!(get_str(&mut cur).is_err());
    }

    #[test]
    fn get_str_empty_buf() {
        let mut cur = BytesMut::new().freeze();
        assert!(get_str(&mut cur).is_err());
    }

    #[test]
    fn deserialize_rejects_bad_magic() {
        let mut buf = BytesMut::new();
        buf.put_u32(0xDEADBEEF);
        buf.put_u16(1);
        buf.put_slice(&[0; 13]); // fill to HEADER_SIZE
        assert!(deserialize_state(buf).is_err());
    }

    #[test]
    fn deserialize_rejects_bad_version() {
        let mut buf = BytesMut::new();
        buf.put_u32(MIGRATION_MAGIC);
        buf.put_u16(99);
        buf.put_slice(&[0; 13]);
        assert!(deserialize_state(buf).is_err());
    }

    #[test]
    fn deserialize_rejects_v1_format() {
        // v1 used to be accepted alongside MIGRATION_VERSION; after dropping
        // legacy support the validator must reject it explicitly.
        let mut buf = BytesMut::new();
        buf.put_u32(MIGRATION_MAGIC);
        buf.put_u16(1);
        buf.put_slice(&[0; 13]); // fill to HEADER_SIZE
        let Err(err) = deserialize_state(buf) else {
            panic!("expected v1 format to be rejected");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("unsupported version 1"),
            "expected v1-rejection error, got: {msg}"
        );
    }

    #[test]
    fn deserialize_rejects_truncated_header() {
        let buf = BytesMut::from(&[0u8; 5][..]);
        assert!(deserialize_state(buf).is_err());
    }

    #[test]
    fn deserialize_rejects_truncated_body() {
        let mut buf = BytesMut::new();
        buf.put_u32(MIGRATION_MAGIC);
        buf.put_u16(MIGRATION_VERSION);
        buf.put_u64(42); // connection_id
        buf.put_i32(1); // secret_key
        buf.put_u8(1); // transaction_mode
                       // missing pool_name, username, etc.
        assert!(deserialize_state(buf).is_err());
    }

    #[test]
    fn deserialize_rejects_excessive_cache_count() {
        let mut buf = BytesMut::new();
        buf.put_u32(MIGRATION_MAGIC);
        buf.put_u16(MIGRATION_VERSION);
        buf.put_u64(1); // connection_id
        buf.put_i32(1); // secret_key
        buf.put_u8(0); // transaction_mode

        put_str(&mut buf, "mydb").unwrap(); // pool_name
        put_str(&mut buf, "user").unwrap(); // username

        buf.put_u16(5432); // port
        buf.put_u8(9); // ip_len
        buf.put_slice(b"127.0.0.1");

        buf.put_u16(0); // 0 server params

        buf.put_u8(1); // prepared_enabled
        buf.put_u8(0); // async_client
        buf.put_u32(u32::MAX); // cache_count = 4 billion

        assert!(deserialize_state(buf).is_err());
    }

    #[test]
    fn deserialize_rejects_negative_prepared_param_count() {
        let mut buf = BytesMut::new();
        buf.put_u32(MIGRATION_MAGIC);
        buf.put_u16(MIGRATION_VERSION);
        buf.put_u64(1); // connection_id
        buf.put_i32(1); // secret_key
        buf.put_u8(0); // transaction_mode

        put_str(&mut buf, "mydb").unwrap();
        put_str(&mut buf, "user").unwrap();

        buf.put_u16(5432);
        buf.put_u8(9);
        buf.put_slice(b"127.0.0.1");

        buf.put_u16(0); // server parameters

        buf.put_u8(1); // prepared_enabled
        buf.put_u8(0); // async_client
        buf.put_u32(1); // cache_count

        buf.put_u8(0); // Named
        put_str(&mut buf, "stmt1").unwrap();
        buf.put_u64(0xABCD);
        let query = "SELECT 1";
        buf.put_u32(query.len() as u32);
        buf.put_slice(query.as_bytes());
        buf.put_i16(-1); // invalid negative num_params

        assert!(deserialize_state(buf).is_err());
    }

    #[test]
    fn deserialize_rejects_unknown_prepared_key_tag() {
        let mut buf = BytesMut::new();
        buf.put_u32(MIGRATION_MAGIC);
        buf.put_u16(MIGRATION_VERSION);
        buf.put_u64(1); // connection_id
        buf.put_i32(1); // secret_key
        buf.put_u8(0); // transaction_mode

        put_str(&mut buf, "mydb").unwrap();
        put_str(&mut buf, "user").unwrap();

        buf.put_u16(5432);
        buf.put_u8(9);
        buf.put_slice(b"127.0.0.1");

        buf.put_u16(0); // server parameters

        buf.put_u8(1); // prepared_enabled
        buf.put_u8(0); // async_client
        buf.put_u32(1); // cache_count

        buf.put_u8(2); // invalid key tag
        buf.put_u64(0xFEED); // old code consumed this as an anonymous key
        buf.put_u64(0xABCD);
        let query = "SELECT 1";
        buf.put_u32(query.len() as u32);
        buf.put_slice(query.as_bytes());
        buf.put_i16(0); // no params
        buf.put_u8(0); // no tls

        let Err(err) = deserialize_state(buf) else {
            panic!("expected unknown prepared key tag to be rejected");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("unknown prepared key tag 2"),
            "unexpected error for malformed migration prepared key tag: {msg}"
        );
    }

    #[test]
    fn deserialize_rejects_unknown_backend_auth_tag() {
        let mut buf = BytesMut::new();
        buf.put_u32(MIGRATION_MAGIC);
        buf.put_u16(MIGRATION_VERSION);
        buf.put_u64(1);
        buf.put_i32(1);
        buf.put_u8(0);

        put_str(&mut buf, "mydb").unwrap();
        put_str(&mut buf, "user").unwrap();

        buf.put_u16(5432);
        buf.put_u8(9);
        buf.put_slice(b"127.0.0.1");

        buf.put_u16(0); // server parameters
        buf.put_u8(1); // prepared_enabled
        buf.put_u8(0); // async_client
        buf.put_u32(0); // cache_count
        buf.put_u8(0); // use_tls
        buf.put_u8(9); // invalid backend-auth tag

        let Err(err) = deserialize_state(buf) else {
            panic!("expected unknown backend-auth tag to be rejected");
        };
        let msg = err.to_string();
        assert!(
            msg.contains("unknown backend auth tag 9"),
            "unexpected error for malformed migration backend-auth tag: {msg}"
        );
    }

    #[test]
    fn migration_frame_len_rejects_oversized_send_lengths() {
        let err = checked_migration_frame_len(
            "state",
            MAX_MIGRATION_STATE_LEN + 1,
            MAX_MIGRATION_STATE_LEN,
        )
        .unwrap_err();
        assert!(err.to_string().contains("state length"));

        let err = checked_migration_frame_len(
            "tls",
            MAX_MIGRATION_TLS_STATE_LEN + 1,
            MAX_MIGRATION_TLS_STATE_LEN,
        )
        .unwrap_err();
        assert!(err.to_string().contains("tls length"));
    }

    #[cfg(unix)]
    #[test]
    fn send_migration_fd_completes_partial_frame_under_buffer_pressure() {
        // a short sendmsg has already
        // transferred the SCM_RIGHTS fd, so the frame must be COMPLETED with
        // follow-up sends, not failed (which would leave the receiver holding a
        // live fd plus an unparseable frame). Force multi-segment sends with a
        // non-blocking sender, a tiny send buffer and a large state, drained by
        // a concurrent reader; assert the receiver reconstructs the exact frame
        // plus a valid fd.
        let sockets = socketpair_for_test();
        let (send_sock, recv_sock) = (sockets[0], sockets[1]);

        // Tiny send buffer + non-blocking sender => the first sendmsg sends a
        // partial frame and returns short, exercising the completion loop.
        let bufsize: libc::c_int = 4096;
        unsafe {
            libc::setsockopt(
                send_sock,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &bufsize as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
            let flags = libc::fcntl(send_sock, libc::F_GETFL);
            libc::fcntl(send_sock, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        let file = File::open("/dev/null").unwrap();
        // SAFETY: file.as_raw_fd() is valid for the lifetime of file.
        let send_fd = unsafe { libc::dup(file.as_raw_fd()) };
        assert!(send_fd >= 0, "dup failed");

        let state_bytes = vec![0xABu8; 256 * 1024];
        let mut payload = MigrationPayload {
            state: BytesMut::from(&state_bytes[..]),
            fd: send_fd,
            tls_state: None,
        };

        // Reader drains the receiver concurrently so the sender's poll(POLLOUT)
        // can make progress (RawFd is Copy; only the main thread closes it).
        let reader = std::thread::spawn(move || recv_migration_fd(recv_sock));

        send_migration_fd(send_sock, &mut payload)
            .expect("send must complete the frame across short writes");
        assert_eq!(payload.fd, -1, "transferred fd must be marked consumed");

        let (received_fd, state, tls_state) = reader
            .join()
            .unwrap()
            .expect("recv must reconstruct the frame");
        assert_eq!(state.len(), state_bytes.len(), "full state must arrive");
        assert_eq!(&state[..], &state_bytes[..], "frame must arrive intact");
        assert!(tls_state.is_none());
        assert!(received_fd >= 0, "a valid fd must be transferred");

        unsafe {
            libc::close(received_fd);
            libc::close(send_sock);
            libc::close(recv_sock);
        }
    }

    fn socketpair_for_test() -> [RawFd; 2] {
        let mut sockets: [libc::c_int; 2] = [-1, -1];
        // SAFETY: socketpair writes two valid fds into sockets on success.
        let rc =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sockets.as_mut_ptr()) };
        assert_eq!(
            rc,
            0,
            "socketpair failed: {}",
            std::io::Error::last_os_error()
        );
        sockets
    }

    fn send_fd_with_bytes_for_test(socket_fd: RawFd, fd_to_send: RawFd, bytes: &[u8]) {
        let iov = libc::iovec {
            iov_base: bytes.as_ptr() as *mut libc::c_void,
            iov_len: bytes.len(),
        };
        // SAFETY: CMSG_SPACE returns the correct buffer size for one RawFd.
        let cmsg_space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
        let mut cmsg_buf = vec![0u8; cmsg_space];
        // SAFETY: zeroed msghdr is a valid initial state for sendmsg.
        let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
        msghdr.msg_iov = &iov as *const _ as *mut _;
        msghdr.msg_iovlen = 1;
        msghdr.msg_control = cmsg_buf.as_mut_ptr() as *mut libc::c_void;
        msghdr.msg_controllen = cmsg_space as _;

        // SAFETY: cmsg buffer is sized for one RawFd and msghdr points to valid buffers.
        let ret = unsafe {
            let cmsg = libc::CMSG_FIRSTHDR(&msghdr);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::copy_nonoverlapping(
                &fd_to_send as *const RawFd as *const u8,
                libc::CMSG_DATA(cmsg),
                std::mem::size_of::<RawFd>(),
            );
            libc::sendmsg(socket_fd, &msghdr, 0)
        };
        assert_eq!(
            ret,
            bytes.len() as isize,
            "sendmsg failed/short: {}",
            std::io::Error::last_os_error()
        );
    }

    fn assert_peer_observes_eof(peer_fd: RawFd) {
        let mut byte = [0u8; 1];
        // SAFETY: recv only writes to the provided one-byte buffer.
        let rc = unsafe {
            libc::recv(
                peer_fd,
                byte.as_mut_ptr() as *mut libc::c_void,
                byte.len(),
                libc::MSG_DONTWAIT,
            )
        };
        assert_eq!(
            rc,
            0,
            "peer fd {peer_fd} did not observe EOF after migrated fd error; rc={rc}, err={}",
            std::io::Error::last_os_error()
        );
    }

    fn minimal_state_with_tls_flag(use_tls: bool) -> BytesMut {
        let mut buf = BytesMut::new();
        buf.put_u32(MIGRATION_MAGIC);
        buf.put_u16(MIGRATION_VERSION);
        buf.put_u64(1);
        buf.put_i32(1);
        buf.put_u8(0);
        put_str(&mut buf, "db").unwrap();
        put_str(&mut buf, "user").unwrap();
        buf.put_u16(6543);
        let ip = "127.0.0.1";
        buf.put_u8(ip.len() as u8);
        buf.put_slice(ip.as_bytes());
        buf.put_u16(0);
        buf.put_u8(0);
        buf.put_u8(0);
        buf.put_u32(0);
        buf.put_u8(use_tls as u8);
        buf
    }

    #[tokio::test]
    async fn reconstruct_client_consumes_fd_on_bad_state() {
        let sockets = socketpair_for_test();
        let fd = sockets[0];
        let client_server_map: ClientServerMap = Arc::new(DashMap::new());

        let err =
            match reconstruct_client(fd, BytesMut::from(&[0u8; 5][..]), client_server_map).await {
                Ok(_) => panic!("bad migration state unexpectedly reconstructed a client"),
                Err(err) => err,
            };

        drop(err);
        assert_peer_observes_eof(sockets[1]);
        close_raw_fd(sockets[1]);
    }

    #[tokio::test]
    async fn reconstruct_client_rejects_tls_flagged_state_on_plain_path() {
        let sockets = socketpair_for_test();
        let fd = sockets[0];
        let client_server_map: ClientServerMap = Arc::new(DashMap::new());

        let err = match reconstruct_client(fd, minimal_state_with_tls_flag(true), client_server_map)
            .await
        {
            Ok(client) => {
                drop(client);
                panic!("TLS-marked migration state unexpectedly reconstructed as plain TCP")
            }
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("TLS migration state"),
            "unexpected error: {err}"
        );
        assert_peer_observes_eof(sockets[1]);
        close_raw_fd(sockets[1]);
    }

    #[tokio::test]
    async fn reconstruct_client_rejects_unix_fd_on_plain_path() {
        let sockets = socketpair_for_test();
        let fd = sockets[0];
        let client_server_map: ClientServerMap = Arc::new(DashMap::new());

        let err =
            match reconstruct_client(fd, minimal_state_with_tls_flag(false), client_server_map)
                .await
            {
                Ok(client) => {
                    drop(client);
                    panic!("AF_UNIX fd unexpectedly reconstructed as a plain TCP client")
                }
                Err(err) => err,
            };

        assert!(
            err.to_string().contains("non-TCP"),
            "unexpected error: {err}"
        );
        assert_peer_observes_eof(sockets[1]);
        close_raw_fd(sockets[1]);
    }

    #[test]
    fn prepare_migration_rejects_unix_socket_fd() {
        let sockets = socketpair_for_test();
        let mut client = migration_test_client(false);
        client.raw_fd = Some(sockets[0]);

        let err = match client.prepare_migration() {
            Ok(payload) => {
                close_raw_fd(payload.fd);
                panic!("AF_UNIX fd unexpectedly prepared for plain TCP migration")
            }
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("non-TCP"),
            "unexpected error: {err}"
        );
        close_raw_fd(sockets[0]);
        close_raw_fd(sockets[1]);
    }

    #[test]
    #[serial_test::serial(current_client_count)]
    fn migrated_client_admission_rejects_over_max_and_restores_counter() {
        use crate::app::server::CURRENT_CLIENT_COUNT;
        use std::sync::atomic::Ordering;

        CURRENT_CLIENT_COUNT.store(1, Ordering::SeqCst);
        assert!(
            try_acquire_migrated_client_count_guard(1).is_none(),
            "migrated client must be rejected when max_connections is already reached"
        );
        assert_eq!(
            CURRENT_CLIENT_COUNT.load(Ordering::SeqCst),
            1,
            "failed migrated admission must drop the temporary guard"
        );

        CURRENT_CLIENT_COUNT.store(0, Ordering::SeqCst);
        let guard = try_acquire_migrated_client_count_guard(1)
            .expect("first migrated client should be admitted");
        assert_eq!(CURRENT_CLIENT_COUNT.load(Ordering::SeqCst), 1);
        drop(guard);
        assert_eq!(CURRENT_CLIENT_COUNT.load(Ordering::SeqCst), 0);
    }

    #[test]
    #[serial_test::serial(current_client_count)]
    fn migrated_client_admission_rejects_over_max_and_closes_fd() {
        use crate::app::server::CURRENT_CLIENT_COUNT;
        use std::sync::atomic::Ordering;

        let sockets = socketpair_for_test();
        CURRENT_CLIENT_COUNT.store(1, Ordering::SeqCst);

        assert!(
            try_acquire_migrated_client_count_guard_or_close_fd(1, sockets[0]).is_none(),
            "migrated fd must be rejected when max_connections is already reached"
        );
        assert_eq!(
            CURRENT_CLIENT_COUNT.load(Ordering::SeqCst),
            1,
            "failed migrated admission must drop the temporary guard"
        );
        assert_peer_observes_eof(sockets[1]);
        close_raw_fd(sockets[1]);
        CURRENT_CLIENT_COUNT.store(0, Ordering::SeqCst);
    }

    #[test]
    fn migration_receiver_admits_before_reconstructing_migrated_state() {
        let src = include_str!("migration.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let receiver_start = impl_src
            .find("pub async fn migration_receiver_task")
            .expect("migration receiver task not found");
        let receiver = &impl_src[receiver_start..];

        let helper_idx = receiver
            .find("try_acquire_migrated_client_count_guard_or_close_fd")
            .expect("migration receiver must use fd-closing admission helper");
        let plain_reconstruct_idx = receiver
            .find("reconstruct_client(")
            .expect("plain migrated reconstruction not found");
        assert!(
            helper_idx < plain_reconstruct_idx,
            "plain migrated fd admission must happen before reconstruct_client mutates process state"
        );

        if let Some(tls_reconstruct_idx) = receiver.find("reconstruct_tls_client(") {
            assert!(
                helper_idx < tls_reconstruct_idx,
                "TLS migrated fd admission must happen before reconstruct_tls_client mutates process state"
            );
        }
    }

    #[test]
    fn migration_receiver_reserves_client_count_before_detaching_reconstruction() {
        let src = include_str!("migration.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let receiver_start = impl_src
            .find("pub async fn migration_receiver_task")
            .expect("migration receiver task not found");
        let receiver = &impl_src[receiver_start..];

        let plain_reconstruct_idx = receiver
            .find("reconstruct_client(")
            .expect("plain migrated reconstruction not found");
        let plain_spawn_idx = receiver[..plain_reconstruct_idx]
            .rfind("tokio::spawn(async move")
            .expect("plain migrated reconstruction must run in a detached task");
        let plain_helper_idx = receiver[..plain_spawn_idx]
            .rfind("try_acquire_migrated_client_count_guard_or_close_fd")
            .expect("plain migrated fd must reserve client count before detached task");
        assert!(
            plain_helper_idx < plain_spawn_idx,
            "plain migrated fd must reserve ClientCountGuard before receiver detaches reconstruction"
        );
        assert!(
            receiver[plain_spawn_idx..plain_reconstruct_idx]
                .contains("let _client_count_guard = client_count_guard"),
            "plain detached reconstruction must keep the pre-reserved ClientCountGuard alive"
        );

        if let Some(tls_reconstruct_idx) = receiver.find("reconstruct_tls_client(") {
            let tls_spawn_idx = receiver[..tls_reconstruct_idx]
                .rfind("tokio::spawn(async move")
                .expect("TLS migrated reconstruction must run in a detached task");
            let tls_helper_idx = receiver[..tls_spawn_idx]
                .rfind("try_acquire_migrated_client_count_guard_or_close_fd")
                .expect("TLS migrated fd must reserve client count before detached task");
            assert!(
                tls_helper_idx < tls_spawn_idx,
                "TLS migrated fd must reserve ClientCountGuard before receiver detaches reconstruction"
            );
            assert!(
                receiver[tls_spawn_idx..tls_reconstruct_idx]
                    .contains("let _client_count_guard = client_count_guard"),
                "TLS detached reconstruction must keep the pre-reserved ClientCountGuard alive"
            );
        }
    }

    #[test]
    fn migration_receiver_receive_failures_are_not_info_only() {
        let src = include_str!("migration.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let receiver_start = impl_src
            .find("pub async fn migration_receiver_task")
            .expect("migration receiver task not found");
        let receiver = &impl_src[receiver_start..];
        let receive_err_idx = receiver
            .find("Ok(Err(e))")
            .expect("migration receiver receive error branch not found");
        let join_err_idx = receiver[receive_err_idx..]
            .find("\n            Err(e)")
            .map(|idx| receive_err_idx + idx)
            .expect("migration receiver join error branch not found");
        let receive_err_branch = &receiver[receive_err_idx..join_err_idx];

        assert!(
            receive_err_branch.contains("migration_receive_error_kind"),
            "receiver must distinguish clean EOF from protocol/IO receive failures"
        );
        assert!(
            receive_err_branch.contains("record_migration_receiver_failure"),
            "non-EOF receive failures must increment an operator-visible counter"
        );
        assert!(
            receive_err_branch.contains("warn!") || receive_err_branch.contains("error!"),
            "non-EOF receive failures must be logged above info level"
        );
        assert!(
            !receive_err_branch.contains(
                "Ok(Err(e)) => {\n                info!(\"migration receiver done: {e}\");"
            ),
            "receive error branch must not immediately treat every error as normal drain"
        );
    }

    #[test]
    fn recv_migration_fd_retries_interrupted_syscalls() {
        let src = include_str!("migration.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap_or(src);
        let recv_start = impl_src
            .find("pub fn recv_migration_fd")
            .expect("recv_migration_fd not found");
        let recv_end = impl_src[recv_start..]
            .find("// ---------------------------------------------------------------------------")
            .map(|idx| recv_start + idx)
            .expect("recv_migration_fd section end not found");
        let recv_fn = &impl_src[recv_start..recv_end];

        assert!(
            recv_fn.contains("recvmsg_retrying_interrupted"),
            "initial recvmsg must retry EINTR instead of aborting migration"
        );
        assert!(
            recv_fn.matches("recv_retrying_interrupted").count() >= 3,
            "state, TLS header, and TLS body reads must retry EINTR"
        );
    }

    #[cfg(not(feature = "tls-migration"))]
    #[test]
    fn prepare_migration_rejects_tls_without_tls_migration_feature() {
        let client = migration_test_client(true);

        let err = match client.prepare_migration() {
            Ok(_) => panic!("TLS client migration unexpectedly succeeded without tls-migration"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("TLS migration unavailable"),
            "unexpected error: {err}"
        );
    }

    #[cfg(all(unix, feature = "tls-migration"))]
    #[test]
    fn prepare_migration_rejects_tls_without_exported_tls_state() {
        let client = migration_test_client(true);

        let err = match client.prepare_migration() {
            Ok(_) => panic!("TLS client migration unexpectedly succeeded without TLS export state"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("TLS migration unavailable"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn recv_migration_fd_rejects_oversized_state_len_before_allocation() {
        let sockets = socketpair_for_test();
        let file = File::open("/dev/null").unwrap();
        // SAFETY: file.as_raw_fd() is valid for the lifetime of file.
        let send_fd = unsafe { libc::dup(file.as_raw_fd()) };
        assert!(send_fd >= 0);

        let frame = ((MAX_MIGRATION_STATE_LEN + 1) as u32).to_be_bytes();
        send_fd_with_bytes_for_test(sockets[0], send_fd, &frame);
        close_raw_fd(send_fd);

        let err = recv_migration_fd(sockets[1]).unwrap_err();
        assert!(err.to_string().contains("state length"));
        close_raw_fd(sockets[0]);
        close_raw_fd(sockets[1]);
    }

    #[test]
    fn recv_migration_fd_rejects_oversized_tls_len_before_allocation() {
        let sockets = socketpair_for_test();
        let file = File::open("/dev/null").unwrap();
        // SAFETY: file.as_raw_fd() is valid for the lifetime of file.
        let send_fd = unsafe { libc::dup(file.as_raw_fd()) };
        assert!(send_fd >= 0);

        let mut frame = Vec::new();
        frame.extend_from_slice(&0u32.to_be_bytes());
        frame.extend_from_slice(&((MAX_MIGRATION_TLS_STATE_LEN + 1) as u32).to_be_bytes());
        send_fd_with_bytes_for_test(sockets[0], send_fd, &frame);
        close_raw_fd(send_fd);

        let err = recv_migration_fd(sockets[1]).unwrap_err();
        assert!(err.to_string().contains("tls length"));
        close_raw_fd(sockets[0]);
        close_raw_fd(sockets[1]);
    }

    #[test]
    fn discard_all_transaction_guard_skips_intercepted_rewrites_in_migration() {
        let src = include_str!("migration.rs");
        let start = src
            .find("fn serialize_prepared_state(")
            .expect("serialize_prepared_state not found");
        let end = src[start..]
            .find("// ---------------------------------------------------------------------------\n// Deserialization")
            .map(|idx| start + idx)
            .expect("deserialization marker not found");
        let body = &src[start..end];

        assert!(
            body.contains("!cached.intercepted_discard_all"),
            "migration must not serialize extended DISCARD ALL rewrite entries \
             unless the transaction guard flag is preserved in the wire format"
        );
    }

    #[test]
    fn serialize_prepared_state_drops_intercepted_discard_all_rewrites() {
        let mut prepared = PreparedStatementState::new(true, 0);
        let intercepted = CachedStatement {
            parse: Arc::new(Parse::from_parts("SELECT 1", &[])),
            hash: 0x11,
            intercepted_discard_all: true,
            set_cleanup_command: None,
            reset_cleanup_command: None,
            async_name: None,
        };
        let normal = CachedStatement {
            parse: Arc::new(Parse::from_parts("SELECT 42", &[])),
            hash: 0x22,
            intercepted_discard_all: false,
            set_cleanup_command: None,
            reset_cleanup_command: None,
            async_name: None,
        };

        let _ = prepared.cache.put(
            PreparedStatementKey::Named("discard_all".to_string()),
            intercepted,
        );
        let _ = prepared
            .cache
            .put(PreparedStatementKey::Named("normal".to_string()), normal);

        let mut buf = BytesMut::new();
        serialize_prepared_state(&mut buf, &prepared).unwrap();

        assert_eq!(buf[0], 1, "prepared cache should remain enabled");
        assert_eq!(buf[1], 0, "test cache is not async");
        let count = u32::from_be_bytes(buf[2..6].try_into().unwrap());
        assert_eq!(count, 1, "intercepted DISCARD ALL rewrite must not migrate");
        let serialized = String::from_utf8_lossy(&buf);
        assert!(serialized.contains("SELECT 42"));
        assert!(!serialized.contains("SELECT 1"));
        assert!(!serialized.contains("discard_all"));
    }

    #[test]
    fn serialize_prepared_state_rejects_growth_past_state_cap_before_writing() {
        let mut prepared = PreparedStatementState::new(true, 0);
        let parse = Arc::new(Parse::from_parts("SELECT 1", &[]));
        let cached = CachedStatement {
            parse,
            hash: 1,
            intercepted_discard_all: false,
            set_cleanup_command: None,
            reset_cleanup_command: None,
            async_name: None,
        };
        let _ = prepared
            .cache
            .put(PreparedStatementKey::Anonymous(1), cached);

        let mut buf = BytesMut::new();
        buf.resize(MAX_MIGRATION_STATE_LEN - 1, 0);
        let before_len = buf.len();

        let err = serialize_prepared_state(&mut buf, &prepared).unwrap_err();

        assert!(err.to_string().contains("migration state length"));
        assert_eq!(
            buf.len(),
            before_len,
            "serializer must reject before extending the migration frame"
        );
    }

    #[test]
    fn serialize_state_rejects_too_many_server_parameters() {
        let mut client = migration_test_client(false);
        for i in 0..=u16::MAX as usize {
            client
                .server_parameters
                .set_param(format!("p{i}"), "v", true);
        }

        let err = client.serialize_state(false).unwrap_err();

        assert!(
            err.to_string().contains("server parameter count"),
            "expected server-parameter count rejection, got: {err}"
        );
    }

    #[test]
    fn serialize_server_parameters_rejects_growth_past_state_cap_before_writing() {
        let mut params = std::collections::HashMap::new();
        params.insert("application_name".to_string(), "x".to_string());
        let mut buf = BytesMut::new();
        buf.resize(MAX_MIGRATION_STATE_LEN - 1, 0);
        let before_len = buf.len();

        let err = serialize_server_parameters(&mut buf, &params).unwrap_err();

        assert!(err.to_string().contains("migration state length"));
        assert_eq!(
            buf.len(),
            before_len,
            "server-parameter serializer must reject before extending the migration frame"
        );
    }

    #[test]
    fn serialize_deserialize_roundtrip_minimal() {
        // Build a minimal serialized state by hand (no Client needed)
        let mut buf = BytesMut::new();
        buf.put_u32(MIGRATION_MAGIC);
        buf.put_u16(MIGRATION_VERSION);
        buf.put_u64(12345); // connection_id
        buf.put_i32(-42); // secret_key
        buf.put_u8(1); // transaction_mode = true

        put_str(&mut buf, "testdb").unwrap();
        put_str(&mut buf, "testuser").unwrap();

        // Address: 192.168.1.1:5432
        buf.put_u16(5432);
        let ip = "192.168.1.1";
        buf.put_u8(ip.len() as u8);
        buf.put_slice(ip.as_bytes());

        // 1 server parameter
        buf.put_u16(1);
        put_str(&mut buf, "application_name").unwrap();
        put_str(&mut buf, "myapp").unwrap();

        // Prepared statements: enabled, not async, 1 entry
        buf.put_u8(1); // enabled
        buf.put_u8(0); // async_client
        buf.put_u32(1); // cache_count

        // Entry: Named("stmt1"), hash=0xABCD, query="SELECT 1", params=[23]
        buf.put_u8(0); // Named
        put_str(&mut buf, "stmt1").unwrap();
        buf.put_u64(0xABCD);
        let query = "SELECT 1";
        buf.put_u32(query.len() as u32);
        buf.put_slice(query.as_bytes());
        buf.put_i16(1); // 1 param
        buf.put_i32(23); // int4 OID

        buf.put_u8(0); // use_tls = false

        let state = deserialize_state(buf).unwrap();
        assert_eq!(state.connection_id, 12345);
        assert_eq!(state.secret_key, -42);
        assert!(state.transaction_mode);
        assert_eq!(state.pool_name, "testdb");
        assert_eq!(state.username, "testuser");
        assert_eq!(state.pool_user, "testuser");
        assert_eq!(state.addr.port(), 5432);
        assert_eq!(state.addr.ip().to_string(), "192.168.1.1");
        assert!(state.prepared_enabled);
        assert!(!state.async_client);
        assert_eq!(state.prepared_entries.len(), 1);
        assert_eq!(
            state.prepared_entries[0].key,
            PreparedStatementKey::Named("stmt1".into())
        );
        assert_eq!(state.prepared_entries[0].hash, 0xABCD);
        assert_eq!(state.prepared_entries[0].query, "SELECT 1");
        assert_eq!(state.prepared_entries[0].param_types, vec![23]);
        assert_eq!(state.last_anonymous_hash, None);
    }

    #[test]
    fn recv_migration_fd_marks_received_fd_close_on_exec() {
        let mut sockets: [libc::c_int; 2] = [-1, -1];
        // SAFETY: socketpair writes two valid fds into sockets on success.
        let rc =
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sockets.as_mut_ptr()) };
        assert_eq!(
            rc,
            0,
            "socketpair failed: {}",
            std::io::Error::last_os_error()
        );

        let file = File::open("/dev/null").unwrap();
        // SAFETY: file.as_raw_fd() is valid for the lifetime of file.
        let send_fd = unsafe { libc::dup(file.as_raw_fd()) };
        assert!(
            send_fd >= 0,
            "dup failed: {}",
            std::io::Error::last_os_error()
        );
        // Force sender-side inheritance; the receiver must set CLOEXEC itself.
        unsafe {
            let flags = libc::fcntl(send_fd, libc::F_GETFD);
            assert!(flags >= 0, "F_GETFD failed");
            assert_eq!(
                libc::fcntl(send_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC),
                0
            );
        }

        let mut payload = MigrationPayload {
            state: BytesMut::from(&b"state"[..]),
            fd: send_fd,
            tls_state: None,
        };
        send_migration_fd(sockets[0], &mut payload).unwrap();

        let (received_fd, state, tls_state) = recv_migration_fd(sockets[1]).unwrap();
        assert_eq!(&state[..], b"state");
        assert!(tls_state.is_none());

        unsafe {
            let flags = libc::fcntl(received_fd, libc::F_GETFD);
            assert!(flags >= 0, "F_GETFD on received fd failed");
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
            libc::close(received_fd);
            libc::close(sockets[0]);
            libc::close(sockets[1]);
        }
    }

    #[tokio::test]
    async fn migration_sender_shutdown_drains_queued_payloads() {
        for attempt in 0..32 {
            let sockets = socketpair_for_test();
            let file = File::open("/dev/null").unwrap();
            // SAFETY: file.as_raw_fd() is valid for the lifetime of file.
            let payload_fd = unsafe { libc::dup(file.as_raw_fd()) };
            assert!(payload_fd >= 0);

            let (tx, rx) = mpsc::channel(1);
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
            tx.send(MigrationPayload {
                state: BytesMut::from(&format!("queued-state-{attempt}").into_bytes()[..]),
                fd: payload_fd,
                tls_state: None,
            })
            .await
            .unwrap();
            drop(shutdown_tx);
            drop(tx);

            let sender = tokio::spawn(migration_sender_task(sockets[0], rx, shutdown_rx));
            sender.await.unwrap();

            let (received_fd, state, tls_state) = recv_migration_fd(sockets[1])
                .unwrap_or_else(|err| panic!("attempt {attempt}: queued payload lost: {err}"));
            assert_eq!(&state[..], format!("queued-state-{attempt}").as_bytes());
            assert!(tls_state.is_none());
            close_raw_fd(received_fd);
            close_raw_fd(sockets[1]);
        }
    }

    #[test]
    fn migration_sender_task_uses_blocking_pool_for_fd_send() {
        let src = include_str!("migration.rs");
        let start = src
            .find("pub async fn migration_sender_task")
            .expect("migration_sender_task not found");
        let body = &src[start..];
        let end = body
            .find("/// Receiver task")
            .expect("receiver task docs should follow sender task");
        let body = &body[..end];

        assert!(
            body.contains("tokio::task::spawn_blocking"),
            "migration_sender_task must not call blocking sendmsg/send inline on a Tokio worker"
        );
    }

    #[test]
    fn serialize_deserialize_ipv6() {
        let mut buf = BytesMut::new();
        buf.put_u32(MIGRATION_MAGIC);
        buf.put_u16(MIGRATION_VERSION);
        buf.put_u64(1);
        buf.put_i32(1);
        buf.put_u8(0);

        put_str(&mut buf, "db").unwrap();
        put_str(&mut buf, "u").unwrap();

        let ipv6 = "::1";
        buf.put_u16(5433);
        buf.put_u8(ipv6.len() as u8);
        buf.put_slice(ipv6.as_bytes());

        buf.put_u16(0); // no params
        buf.put_u8(0); // prepared disabled
        buf.put_u8(0); // not async
        buf.put_u32(0); // no cache entries
        buf.put_u8(0); // no tls

        let state = deserialize_state(buf).unwrap();
        assert_eq!(state.addr.ip().to_string(), "::1");
        assert_eq!(state.addr.port(), 5433);
    }

    /// Mirrors the call-site in PreparedStatementCache::get_or_insert at
    /// the slow path: an Anonymous entry rebuilt by reconstruct_prepared_state
    /// goes through `parse.clone().intern_query(hash, is_anonymous)` with
    /// is_anonymous=true, so the text must land in ANON_INTERNER, not NAMED.
    #[test]
    #[serial_test::serial(query_interner)]
    fn migration_path_routes_anonymous_to_anon_interner() {
        use crate::server::{anon_entry_for_test, named_entry_for_test, reset_interners_for_test};

        reset_interners_for_test();
        let parse = Parse::from_parts("select 99::int", &[]);
        let hash: u64 = 0xCAFE;
        let _interned = parse.clone().intern_query(hash, true);

        assert!(
            anon_entry_for_test(hash).is_some(),
            "must land in ANON_INTERNER"
        );
        assert!(
            named_entry_for_test(hash).is_none(),
            "must NOT land in NAMED_INTERNER"
        );
    }

    /// Mirror for the named branch: Named entry from migration goes into
    /// NAMED_INTERNER.
    #[test]
    #[serial_test::serial(query_interner)]
    fn migration_path_routes_named_to_named_interner() {
        use crate::server::{anon_entry_for_test, named_entry_for_test, reset_interners_for_test};

        reset_interners_for_test();
        let parse = Parse::from_parts("select 100::int", &[]);
        let hash: u64 = 0xBEEF;
        let _interned = parse.clone().intern_query(hash, false);

        assert!(
            named_entry_for_test(hash).is_some(),
            "must land in NAMED_INTERNER"
        );
        assert!(
            anon_entry_for_test(hash).is_none(),
            "must NOT land in ANON_INTERNER"
        );
    }

    #[test]
    fn serialize_deserialize_anonymous_prepared() {
        let mut buf = BytesMut::new();
        buf.put_u32(MIGRATION_MAGIC);
        buf.put_u16(MIGRATION_VERSION);
        buf.put_u64(1);
        buf.put_i32(1);
        buf.put_u8(0);
        put_str(&mut buf, "db").unwrap();
        put_str(&mut buf, "u").unwrap();
        buf.put_u16(5432);
        buf.put_u8(9);
        buf.put_slice(b"127.0.0.1");
        buf.put_u16(0);

        buf.put_u8(1); // enabled
        buf.put_u8(0); // not async
        buf.put_u32(1); // 1 entry

        // Anonymous entry
        buf.put_u8(1); // Anonymous
        buf.put_u64(0xDEAD); // key_hash
        buf.put_u64(0xDEAD); // hash
        let q = "SELECT $1";
        buf.put_u32(q.len() as u32);
        buf.put_slice(q.as_bytes());
        buf.put_i16(0); // no params

        buf.put_u8(0); // no tls

        let state = deserialize_state(buf).unwrap();
        assert_eq!(state.prepared_entries.len(), 1);
        assert_eq!(
            state.prepared_entries[0].key,
            PreparedStatementKey::Anonymous(0xDEAD)
        );
        assert_eq!(state.last_anonymous_hash, None);
    }

    #[test]
    fn deserialize_preserves_last_anonymous_hash_extension() {
        let mut buf = BytesMut::new();
        buf.put_u32(MIGRATION_MAGIC);
        buf.put_u16(MIGRATION_VERSION);
        buf.put_u64(1);
        buf.put_i32(1);
        buf.put_u8(0);
        put_str(&mut buf, "db").unwrap();
        put_str(&mut buf, "u").unwrap();
        buf.put_u16(5432);
        buf.put_u8(9);
        buf.put_slice(b"127.0.0.1");
        buf.put_u16(0);

        buf.put_u8(1); // enabled
        buf.put_u8(0); // not async
        buf.put_u32(1); // 1 entry
        buf.put_u8(1); // Anonymous
        buf.put_u64(0xDEAD); // key_hash
        buf.put_u64(0xDEAD); // hash
        let q = "SELECT $1";
        buf.put_u32(q.len() as u32);
        buf.put_slice(q.as_bytes());
        buf.put_i16(0); // no params

        buf.put_u8(0); // no tls
        buf.put_u8(0); // no backend auth
        buf.put_u8(MIGRATION_EXT_LAST_ANONYMOUS_HASH);
        buf.put_u8(1); // present
        buf.put_u64(0xDEAD);

        let state = deserialize_state(buf).unwrap();
        assert_eq!(state.last_anonymous_hash, Some(0xDEAD));
        assert_eq!(state.pool_user, "u");
    }

    #[test]
    fn last_anonymous_hash_extension_roundtrip() {
        let mut buf = BytesMut::new();
        put_last_anonymous_hash_extension(&mut buf, Some(0xBEEF));
        assert_eq!(
            get_last_anonymous_hash_extension(&mut buf).unwrap(),
            Some(0xBEEF)
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn pool_user_extension_roundtrip_preserves_route_user() {
        let mut buf = BytesMut::new();
        put_pool_user_extension(&mut buf, "shared_backend").unwrap();

        assert_eq!(
            get_pool_user_extension(&mut buf, "authenticated_user").unwrap(),
            "shared_backend"
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn pool_config_hash_extension_roundtrip_preserves_sender_identity() {
        let mut buf = BytesMut::new();
        put_pool_config_hash_extension(&mut buf, Some(0xCAFE_BABE_DEAD_BEEF)).unwrap();

        assert_eq!(
            get_pool_config_hash_extension(&mut buf).unwrap(),
            Some(0xCAFE_BABE_DEAD_BEEF)
        );
        assert!(buf.is_empty());

        let mut absent = BytesMut::new();
        put_pool_config_hash_extension(&mut absent, None).unwrap();
        assert_eq!(get_pool_config_hash_extension(&mut absent).unwrap(), None);
        assert!(absent.is_empty());
    }

    #[test]
    fn serialize_state_writes_route_pool_user_from_cached_pool_id() {
        let mut client = migration_test_client(false);
        client.username = "authenticated_user".to_string();
        client.cached_pool_id = PoolIdentifier::new("db", "shared_backend");

        let state = deserialize_state(client.serialize_state(false).unwrap()).unwrap();

        assert_eq!(state.username, "authenticated_user");
        assert_eq!(state.pool_user, "shared_backend");
    }
}
