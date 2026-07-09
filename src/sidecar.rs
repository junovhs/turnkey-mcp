//! Resident owner / sidecar runtime.
//!
//! Copy-first extraction source: `origin/ishoo/src/mcp/transport.rs`, kept at
//! parity with Ishoo's current transport (FIX-150/153/154/162/168, FEAT-34).
//! This module keeps the core owner pattern: loopback endpoint, tokened owner
//! requests, crash-safe singleton lock, stale-owner retirement on rebuild,
//! build fingerprinting, endpoint reassertion, bounded owner lifetime with
//! idle-reap, and dead-owner recovery that can never spawn a rival writer.
//!
//! ## Lifecycle
//!
//! - A client (usually the stdio MCP process) calls [`ensure_owner_process`]:
//!   attach to a live same-build owner, retire a live different-build owner,
//!   or spawn a fresh one and wait for its registration.
//! - The owner process (the app's hidden `mcp-owner`-style subcommand) calls
//!   [`run_owner_server`] with a handler (usually `McpServer::handle_line`).
//!   It holds an exclusive OS advisory lock for its whole lifetime, so exactly
//!   one owner per workspace can exist — singleton by construction, and a
//!   crashed owner's lock vanishes with it (no stale-lock wedge).
//! - When a mutation cannot reach the owner mid-session, the server calls
//!   [`recover_owner`]: a live-but-unreachable owner is never duplicated
//!   (fail closed); only a genuinely dead one is cleared and re-elected.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OwnerEndpoint {
    pub addr: SocketAddr,
    pub token: String,
    pub pid: u32,
    /// The build fingerprint of the owner process. A client whose own build
    /// differs retires this owner and elects a fresh one, so a reinstall/rebuild
    /// takes effect without a manual restart. `serde(default)` = empty for an
    /// old registration, which mismatches any real fingerprint and is retired
    /// on first contact.
    #[serde(default)]
    pub fingerprint: String,
}

/// A hook the owner runs right before it exits (idle-reap or a shutdown
/// request): drain in-flight serialized work so an exit is never observed as a
/// client-visible error. An interrupted mutation should be crash-safe anyway;
/// draining keeps routine lifecycle invisible.
pub type DrainHook = Arc<dyn Fn() + Send + Sync + 'static>;

#[derive(Clone)]
pub struct SidecarConfig {
    pub app_name: String,
    /// The APP's version, folded into the build fingerprint. Using the app
    /// version (not this library's) means an app release bump retires stale
    /// owners even when the binary metadata heuristic is inconclusive.
    pub app_version: String,
    pub workspace_root: PathBuf,
    pub cache_dir: PathBuf,
    pub owner_args: Vec<String>,
    pub owner_bin_name: String,
    /// A path whose disappearance means this owner has nothing left to serve
    /// (e.g. the app's state dir). The owner watchdog exits the process when it
    /// goes missing, so an owner for a deleted/temp workspace never lingers.
    /// Defaults to `workspace_root`.
    pub liveness_path: Option<PathBuf>,
    /// How long the owner may sit with no client request before it exits
    /// (bounded lifetime). A new session re-spawns it on demand and
    /// `recover_owner` re-elects if a client races the exit, so idle-reaping is
    /// self-healing, not a lost writer. Without a bound, a detached owner would
    /// run forever, holding the installed binary locked (the FIX-162 class).
    pub idle_timeout: Duration,
    /// Env var name that overrides `idle_timeout` in milliseconds (used by
    /// behavior tests). `None` disables the override.
    pub idle_timeout_env: Option<String>,
    /// Drain hook run before an owner exit (idle-reap or shutdown request).
    pub drain: Option<DrainHook>,
}

impl std::fmt::Debug for SidecarConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SidecarConfig")
            .field("app_name", &self.app_name)
            .field("app_version", &self.app_version)
            .field("workspace_root", &self.workspace_root)
            .field("cache_dir", &self.cache_dir)
            .field("owner_args", &self.owner_args)
            .field("owner_bin_name", &self.owner_bin_name)
            .field("liveness_path", &self.liveness_path)
            .field("idle_timeout", &self.idle_timeout)
            .field("idle_timeout_env", &self.idle_timeout_env)
            .field("drain", &self.drain.as_ref().map(|_| "<hook>"))
            .finish()
    }
}

impl SidecarConfig {
    pub fn new(
        app_name: impl Into<String>,
        workspace_root: impl Into<PathBuf>,
        cache_dir: impl Into<PathBuf>,
    ) -> Self {
        let app_name = app_name.into();
        Self {
            owner_bin_name: app_name.clone(),
            app_name,
            app_version: "0".to_string(),
            workspace_root: workspace_root.into(),
            cache_dir: cache_dir.into(),
            owner_args: vec!["mcp-owner".to_string()],
            liveness_path: None,
            idle_timeout: Duration::from_secs(600),
            idle_timeout_env: None,
            drain: None,
        }
    }

    pub fn app_version(mut self, version: impl Into<String>) -> Self {
        self.app_version = version.into();
        self
    }

    pub fn owner_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.owner_args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn owner_bin_name(mut self, name: impl Into<String>) -> Self {
        self.owner_bin_name = name.into();
        self
    }

    pub fn liveness_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.liveness_path = Some(path.into());
        self
    }

    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    pub fn idle_timeout_env(mut self, var: impl Into<String>) -> Self {
        self.idle_timeout_env = Some(var.into());
        self
    }

    pub fn drain(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.drain = Some(Arc::new(hook));
        self
    }

    pub fn endpoint_path(&self) -> PathBuf {
        self.cache_dir.join("mcp-owner.json")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.cache_dir.join("mcp-owner.lock")
    }

    fn effective_idle_timeout(&self) -> Duration {
        self.idle_timeout_env
            .as_deref()
            .and_then(|var| std::env::var(var).ok())
            .and_then(|raw| raw.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or(self.idle_timeout)
    }

    fn run_drain(&self) {
        if let Some(drain) = &self.drain {
            drain();
        }
    }
}

#[derive(Deserialize, Serialize)]
struct OwnerRequest {
    token: String,
    line: String,
}

#[derive(Deserialize, Serialize)]
struct OwnerResponse {
    response: Option<String>,
}

/// Outcome of trying to recover a writable resident owner after a mutation
/// could not reach the one in use.
pub enum OwnerRecovery {
    /// A live owner is reachable now — either a fresh one we spawned, or one
    /// another process registered since. Retry the write here.
    Reelected(OwnerEndpoint),
    /// The owner we were using is unreachable but its process is still alive: a
    /// live single writer mid-blip. Never spawn a second writer — fail the
    /// write closed and let the caller retry against the same owner.
    LiveButUnreachable,
    /// No live owner and a fresh one could not be elected. Carries the reason.
    Down(String),
}

/// Start or attach to the resident owner for this workspace: attach to a live
/// same-build owner; gracefully retire a live different-build owner (so a
/// rebuild/reinstall takes effect on the next session instead of a stale owner
/// serving the old binary forever); else spawn a fresh owner and wait for its
/// registration.
pub fn ensure_owner_process(config: &SidecarConfig) -> Result<OwnerEndpoint, String> {
    if let Some(endpoint) = read_endpoint(config) {
        if send_line(&endpoint, r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#).is_ok() {
            if endpoint.fingerprint == current_build_fingerprint(config) {
                return Ok(endpoint);
            }
            retire_stale_owner(&endpoint);
        }
    }

    let _ = fs::remove_file(config.endpoint_path());
    let exe = resolve_owner_exe(&config.owner_bin_name)?;
    Command::new(exe)
        .args(&config.owner_args)
        // Pin the child's working directory to the (existing) workspace root. A
        // client that inherited a now-deleted cwd would otherwise fail the
        // spawn with ENOENT before the owner ever starts.
        .current_dir(&config.workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|mut child| {
            // Reap the owner when it exits (bounded lifetime, retirement,
            // crash). An unreaped child lingers as a zombie of this long-lived
            // process, and a zombie pid still passes kill(pid, 0) — which reads
            // as a live owner and wedges every write behind "live writer
            // mid-blip". The thread parks in wait() and vanishes with the
            // child; if we exit first, the owner reparents to init, which
            // reaps it.
            thread::spawn(move || {
                let _ = child.wait();
            });
        })
        .map_err(|e| format!("failed to spawn resident MCP owner: {e}"))?;

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(endpoint) = read_endpoint(config).and_then(|endpoint| {
            send_line(&endpoint, r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#)
                .ok()
                .map(|_| endpoint)
        }) {
            return Ok(endpoint);
        }
        if Instant::now() >= deadline {
            return Err("resident MCP owner did not publish a usable endpoint".to_string());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Recover a writable owner after `tried` became unreachable mid-session. PID
/// liveness — not just a failed socket ping — decides whether to spawn: a
/// still-alive owner process is a live writer having a blip and must never be
/// duplicated (the second-writer hole); only a genuinely dead owner is cleared
/// from the registration and replaced.
pub fn recover_owner(config: &SidecarConfig, tried: &OwnerEndpoint) -> OwnerRecovery {
    // Whoever is registered and answering a ping right now is the live owner —
    // a fresh one, a restarted app's, or ours after a transient blip.
    if let Some(current) = read_endpoint(config) {
        if send_line(&current, r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#).is_ok() {
            return OwnerRecovery::Reelected(current);
        }
    }
    if process_is_alive(tried.pid) {
        return OwnerRecovery::LiveButUnreachable;
    }
    // The owner process is gone. Drop the stale registration so no client keeps
    // dialing a corpse, then elect a fresh resident writer.
    let _ = fs::remove_file(config.endpoint_path());
    match ensure_owner_process(config) {
        Ok(endpoint) => OwnerRecovery::Reelected(endpoint),
        Err(error) => OwnerRecovery::Down(error),
    }
}

/// Run the owner process until it idle-reaps or is asked to shut down. Apps
/// call this from their hidden owner subcommand, run their own state startup
/// FIRST (sync, background workers), and pass `server.handle_line` as the
/// handler.
///
/// Singleton by construction: an exclusive OS advisory lock is held for the
/// process's whole lifetime BEFORE any owner work. The kernel releases it the
/// instant the process dies, so election is crash-safe: a dead owner's lock
/// vanishes with it, and a live owner's lock cannot be taken by a second owner
/// process — that one exits cleanly (the client that spawned it finds and uses
/// the existing endpoint) rather than become a rival writer.
pub fn run_owner_server(
    config: SidecarConfig,
    init: impl FnOnce() -> Result<(), String>,
    handler: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
) -> Result<(), String> {
    let _owner_lock = match OwnerLock::try_acquire(&config)? {
        Some(lock) => lock,
        None => return Ok(()),
    };

    // App state startup (sync, background workers) runs AFTER the singleton
    // lock is won and BEFORE the endpoint exists: a doomed second owner must
    // never run rival startup writes, and no client can reach us before the
    // app's state is ready.
    init()?;

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("failed to bind resident MCP owner socket: {e}"))?;
    let endpoint = OwnerEndpoint {
        addr: listener
            .local_addr()
            .map_err(|e| format!("failed to read resident MCP owner address: {e}"))?,
        token: new_token(),
        pid: std::process::id(),
        fingerprint: current_build_fingerprint(&config),
    };
    write_endpoint(&config, &endpoint)?;
    // Only the lock holder ever reaches here, so it is the sole author of the
    // endpoint registration. Keep it authoritative: re-assert it if a racing
    // client removed it, and exit if the state it serves disappears.
    spawn_owner_watchdog(config.clone(), endpoint.clone());

    // The owner is spawned DETACHED (null stdio, survives the MCP server that
    // spawned it) and has no parent to watch — so an unbounded accept loop
    // would run forever, holding the installed binary locked and accumulating
    // one orphan per build/session. Bound its lifetime: idle-reap when no
    // client request has arrived for the idle timeout. Exit drains in-flight
    // serialized work first and the OS releases the singleton lock on exit.
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("failed to set resident MCP owner socket non-blocking: {e}"))?;
    let handler = Arc::new(handler);
    let idle_timeout = config.effective_idle_timeout();
    let mut last_activity = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                last_activity = Instant::now();
                let token = endpoint.token.clone();
                let handler = handler.clone();
                let config = config.clone();
                thread::spawn(move || {
                    let _ = handle_owner_stream(&config, &token, stream, move |line| handler(line));
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if last_activity.elapsed() >= idle_timeout {
                    config.run_drain();
                    std::process::exit(0);
                }
                thread::sleep(idle_poll_interval(idle_timeout));
            }
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// Poll cadence for the idle-reap accept loop: frequent enough to reap promptly
/// after the timeout, capped so a normal 10-minute timeout costs ~1 wakeup/sec.
fn idle_poll_interval(idle_timeout: Duration) -> Duration {
    (idle_timeout / 20).clamp(Duration::from_millis(50), Duration::from_secs(1))
}

/// Classified owner-socket failure (FIX-169).
///
/// A mutation that was fully flushed to the owner may already be committed when
/// the response channel dies (Windows 10054/10060, mid-read reset, etc.). Those
/// failures must not be treated as "write refused" and must not re-send the
/// mutation — that is the duplicate-record / double-land path.
#[derive(Debug, Clone)]
pub enum OwnerTransportError {
    /// Connect failed before any request bytes left the client.
    Connect(String),
    /// Request may not have been fully delivered to the owner.
    Write(String),
    /// Request was flushed; the owner may have applied the mutation.
    ResponseLost(String),
    /// Response bytes arrived but could not be parsed.
    MalformedResponse(String),
}

impl OwnerTransportError {
    /// True when the request left the client and a committed write is possible.
    pub fn may_have_committed(&self) -> bool {
        matches!(
            self,
            Self::ResponseLost(_) | Self::MalformedResponse(_)
        )
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Connect(m)
            | Self::Write(m)
            | Self::ResponseLost(m)
            | Self::MalformedResponse(m) => m,
        }
    }
}

impl std::fmt::Display for OwnerTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for OwnerTransportError {}

pub fn send_line(
    endpoint: &OwnerEndpoint,
    line: &str,
) -> Result<Option<String>, OwnerTransportError> {
    let mut stream = TcpStream::connect_timeout(&endpoint.addr, Duration::from_secs(1))
        .map_err(|e| {
            OwnerTransportError::Connect(format!("failed to connect to resident MCP owner: {e}"))
        })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| {
            OwnerTransportError::Write(format!("failed to set MCP owner read timeout: {e}"))
        })?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| {
            OwnerTransportError::Write(format!("failed to set MCP owner write timeout: {e}"))
        })?;
    let request = OwnerRequest {
        token: endpoint.token.clone(),
        line: line.to_string(),
    };
    serde_json::to_writer(&mut stream, &request).map_err(|e| {
        OwnerTransportError::Write(format!("failed to encode MCP owner request: {e}"))
    })?;
    stream
        .write_all(b"\n")
        .map_err(|e| {
            OwnerTransportError::Write(format!("failed to write MCP owner request: {e}"))
        })?;
    stream.flush().map_err(|e| {
        OwnerTransportError::Write(format!("failed to flush MCP owner request: {e}"))
    })?;

    // From here the owner may already be applying the mutation. A lost reply is
    // ambiguous — never re-send the same mutation as if the write were refused.
    let mut raw = String::new();
    BufReader::new(stream).read_line(&mut raw).map_err(|e| {
        OwnerTransportError::ResponseLost(format!("failed to read MCP owner response: {e}"))
    })?;
    let response: OwnerResponse = serde_json::from_str(raw.trim_end()).map_err(|e| {
        OwnerTransportError::MalformedResponse(format!("malformed MCP owner response: {e}"))
    })?;
    Ok(response.response)
}

fn handle_owner_stream(
    config: &SidecarConfig,
    token: &str,
    stream: TcpStream,
    handler: impl Fn(&str) -> Option<String>,
) -> Result<(), String> {
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|e| format!("failed to clone MCP owner stream: {e}"))?,
    );
    let mut raw = String::new();
    reader
        .read_line(&mut raw)
        .map_err(|e| format!("failed to read MCP owner request: {e}"))?;
    let request: OwnerRequest = serde_json::from_str(raw.trim_end())
        .map_err(|e| format!("malformed MCP owner request: {e}"))?;

    // Graceful upgrade handoff: a token-authenticated `owner/shutdown` retires
    // this owner so a newer-build client can elect a replacement. Ack first (so
    // the client sees a clean handoff), then drain any in-flight serialized
    // work, and exit — the OS releases the singleton lock on exit.
    if request.token == token && line_method(&request.line).as_deref() == Some("owner/shutdown") {
        let ack = Some(r#"{"jsonrpc":"2.0","result":{"status":"shutting_down"}}"#.to_string());
        let mut writer = stream;
        serde_json::to_writer(&mut writer, &OwnerResponse { response: ack })
            .map_err(|e| format!("failed to encode MCP owner response: {e}"))?;
        writer
            .write_all(b"\n")
            .map_err(|e| format!("failed to write MCP owner response: {e}"))?;
        let _ = writer.flush();
        config.run_drain();
        std::process::exit(0);
    }

    let response = if request.token == token {
        handler(&request.line)
    } else {
        Some(crate::response::error_frame(
            serde_json::Value::Null,
            crate::types::INVALID_REQUEST,
            "Invalid resident MCP owner token",
        ))
    };

    let mut writer = stream;
    serde_json::to_writer(&mut writer, &OwnerResponse { response })
        .map_err(|e| format!("failed to encode MCP owner response: {e}"))?;
    writer
        .write_all(b"\n")
        .map_err(|e| format!("failed to write MCP owner response: {e}"))?;
    Ok(())
}

fn read_endpoint(config: &SidecarConfig) -> Option<OwnerEndpoint> {
    let raw = fs::read_to_string(config.endpoint_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_endpoint(config: &SidecarConfig, endpoint: &OwnerEndpoint) -> Result<(), String> {
    let path = config.endpoint_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create MCP owner cache dir: {e}"))?;
    }
    let text = serde_json::to_string(endpoint)
        .map_err(|e| format!("failed to encode MCP owner endpoint: {e}"))?;
    fs::write(&path, text).map_err(|e| format!("failed to write MCP owner endpoint: {e}"))
}

/// The lock holder's registration watchdog. Because only the lock holder runs
/// it, re-asserting is always correct — no rival owner can exist to fight over
/// the file:
///  - Re-write our endpoint if it goes missing or is overwritten (a racing
///    client removes the registration before spawning; the doomed second owner
///    exits without writing, so without this the live owner's registration
///    would stay gone).
///  - Exit the process when the state it exists to serve is gone — an owner
///    for a deleted / temp-dir workspace is dead weight; its lock and socket
///    should die with the state.
fn spawn_owner_watchdog(config: SidecarConfig, endpoint: OwnerEndpoint) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(1));
        let liveness = config
            .liveness_path
            .clone()
            .unwrap_or_else(|| config.workspace_root.clone());
        if !liveness.exists() {
            std::process::exit(0);
        }
        let ours = read_endpoint(&config).is_some_and(|cur| cur.pid == endpoint.pid);
        if !ours {
            let _ = write_endpoint(&config, &endpoint);
        }
    });
}

/// An exclusive, OS-advisory lock on the workspace's owner-lock file, held for
/// the whole lifetime of the resident owner. The kernel releases it the instant
/// the holding process dies, so exactly one owner per workspace can hold it: a
/// second one fails to acquire and exits instead of becoming a rival writer,
/// and a crashed owner leaves no stale lock to wedge the next election.
struct OwnerLock {
    // The open, locked file. Kept alive for the process lifetime; the advisory
    // lock is bound to this open file description and releases when it closes.
    _file: fs::File,
}

impl OwnerLock {
    /// Try to take the singleton lock without blocking. `Ok(Some)` = acquired
    /// (we are the one owner); `Ok(None)` = another live owner already holds
    /// it; `Err` = the lock file could not even be opened (a real filesystem
    /// fault, not contention).
    fn try_acquire(config: &SidecarConfig) -> Result<Option<Self>, String> {
        let path = config.lock_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create owner lock dir: {e}"))?;
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| format!("failed to open owner lock file {}: {e}", path.display()))?;
        match try_lock_exclusive_nonblocking(&file) {
            Ok(true) => Ok(Some(OwnerLock { _file: file })),
            Ok(false) => Ok(None),
            Err(e) => Err(format!("failed to acquire owner lock: {e}")),
        }
    }
}

/// Non-blocking exclusive whole-file advisory lock via `flock(2)`. The lock is
/// bound to the open file description and the kernel drops it on fd close /
/// process death — precisely the crash-safe singleton primitive we want.
/// `Ok(false)` means another open description already holds it (contention).
#[cfg(unix)]
fn try_lock_exclusive_nonblocking(file: &fs::File) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    // EWOULDBLOCK (== EAGAIN) is the "already locked" signal for LOCK_NB.
    match err.raw_os_error() {
        Some(code) if code == libc::EWOULDBLOCK => Ok(false),
        _ => Err(err),
    }
}

/// Windows analogue via `LockFileEx` with `LOCKFILE_FAIL_IMMEDIATELY`
/// (non-blocking) and `LOCKFILE_EXCLUSIVE_LOCK`. The lock is released when the
/// handle closes / the process exits, the same crash-safe property `flock`
/// gives on Unix. A lock-violation error means another owner holds it; any
/// other failure is treated conservatively as "not acquired" so we never start
/// a rival writer on an ambiguous result.
#[cfg(windows)]
fn try_lock_exclusive_nonblocking(file: &fs::File) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{
        LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(code) if code == ERROR_LOCK_VIOLATION as i32 => Ok(false),
        _ => Ok(false),
    }
}

/// Ask a live owner running a different build to shut down, then wait briefly
/// for it to exit so its singleton lock is released before we elect the
/// replacement.
fn retire_stale_owner(endpoint: &OwnerEndpoint) {
    let _ = send_line(
        endpoint,
        r#"{"jsonrpc":"2.0","id":0,"method":"owner/shutdown"}"#,
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !process_is_alive(endpoint.pid) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// The JSON-RPC `method` of a raw owner-request line, or `None` if
/// absent/unparseable — used to route control-plane methods (`owner/shutdown`)
/// before the MCP dispatch.
fn line_method(line: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_string))
}

fn new_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

/// Resolve the executable to spawn the resident owner with. `current_exe()` is
/// the correct binary, but after an in-place replacement (e.g. `cargo install`)
/// the running process's original path is unlinked: on Linux `current_exe()`
/// then returns a `<path> (deleted)` path that does not exist, so spawning it
/// fails with ENOENT and wedges every write. Resolution order: the live
/// `current_exe()` if it still exists; else the same path with the
/// " (deleted)" marker stripped (the replacement usually landed back there);
/// else a `PATH` lookup of the binary name; else a clear error.
fn resolve_owner_exe(owner_bin_name: &str) -> Result<PathBuf, String> {
    let current = std::env::current_exe().ok();
    if let Some(exe) = current.as_ref() {
        if exe.exists() {
            return Ok(exe.clone());
        }
        if let Some(replaced) = strip_deleted_marker(exe) {
            if replaced.exists() {
                return Ok(replaced);
            }
        }
    }
    let name = current
        .as_ref()
        .and_then(|e| e.file_name())
        .map(|n| {
            n.to_string_lossy()
                .trim_end_matches(" (deleted)")
                .to_string()
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| owner_bin_name.to_string());
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(&name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(format!(
        "failed to locate an executable to spawn the resident MCP owner: current_exe is \
         unavailable/deleted and '{name}' was not found on PATH"
    ))
}

fn strip_deleted_marker(exe: &Path) -> Option<PathBuf> {
    exe.to_str()?.strip_suffix(" (deleted)").map(PathBuf::from)
}

/// The running build's owner fingerprint: the APP version plus the resolved
/// owner binary's size and mtime. Computed from `resolve_owner_exe` (not the
/// raw `current_exe`, which may be an unlinked "(deleted)" path after a
/// reinstall), so a client with a replaced binary and the owner it spawns from
/// the *same* on-disk binary agree on the fingerprint — otherwise the two
/// would disagree forever and retire in a loop. Two different builds yield
/// different signatures, so a stale owner is always retired.
fn current_build_fingerprint(config: &SidecarConfig) -> String {
    let exe_sig = resolve_owner_exe(&config.owner_bin_name)
        .ok()
        .and_then(|path| fs::metadata(&path).ok())
        .map(|meta| {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{}-{}", meta.len(), mtime)
        })
        .unwrap_or_default();
    format!("{}+{exe_sig}", config.app_version)
}

/// Whether a process with `pid` is currently alive. Deliberately conservative:
/// when liveness cannot be determined it returns `true`, so a live owner is
/// never mistaken for dead — a false-dead would spawn a second resident
/// writer. Only a definitive "no such process" (or a corpse) reads as dead.
#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        // An exited-but-unreaped child (zombie) still passes kill(pid, 0) — the
        // pid exists until its parent wait()s. A zombie can never answer the
        // owner socket or hold work again, so reporting it alive turns the
        // "live writer mid-blip" refusal into a permanent wedge. A corpse is
        // dead, not alive.
        return !process_is_zombie(pid);
    }
    matches!(std::io::Error::last_os_error().raw_os_error(), Some(code) if code == libc::EPERM)
}

/// Whether `pid` is a zombie — exited, awaiting its parent's wait(). Linux
/// reads the state field of `/proc/<pid>/stat`; the field follows the
/// parenthesized comm, which may itself contain spaces or `)`, so parse from
/// the LAST `)`. Other unixes have no /proc contract; they return false
/// (conservatively alive), same as an unreadable stat.
#[cfg(unix)]
fn process_is_zombie(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        stat.rsplit(')')
            .next()
            .and_then(|rest| rest.split_whitespace().next())
            == Some("Z")
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

/// A terminated Windows process keeps a kernel object while a handle to it
/// exists, so `OpenProcess` can SUCCEED for a dead PID — a non-null handle is
/// not proof of life. Confirm with the exit code: a still-running process
/// reports STILL_ACTIVE (259); any settled exit code means dead.
#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // Only ERROR_INVALID_PARAMETER proves the PID has no live process;
            // any other failure (e.g. ACCESS_DENIED) is treated as alive to
            // avoid a false-dead that would spawn a rival writer.
            return GetLastError() != ERROR_INVALID_PARAMETER;
        }
        let mut exit_code: u32 = 0;
        let read_ok = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        read_ok == 0 || exit_code == STILL_ACTIVE
    }
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

/// Test-support helpers for apps that need to exercise owner recovery paths in
/// their own suites (an unreachable registration, a scripted in-thread owner).
/// Not part of the stability contract for production use.
pub mod test_support {
    use super::*;

    /// An endpoint whose address has no listener, so a connect is refused —
    /// the "resident owner unreachable" shape. Binds an ephemeral port to
    /// reserve a real address, then drops the listener.
    pub fn unreachable_endpoint(config: &SidecarConfig) -> OwnerEndpoint {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind ephemeral port");
        let addr = listener.local_addr().expect("read ephemeral addr");
        drop(listener);
        OwnerEndpoint {
            addr,
            token: new_token(),
            pid: std::process::id(),
            fingerprint: current_build_fingerprint(config),
        }
    }

    /// Persist `endpoint` as the resident-owner registration, so a recovery
    /// path can find and ping it.
    pub fn write_endpoint(config: &SidecarConfig, endpoint: &OwnerEndpoint) {
        super::write_endpoint(config, endpoint).expect("write test owner registration");
    }

    /// Run a token-authenticated owner loop on a background thread of THIS
    /// process (no child process), answering with `handler`.
    pub fn start_owner_thread(
        config: SidecarConfig,
        handler: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Result<OwnerEndpoint, String> {
        start_owner_thread_with(config, handler, false)
    }

    /// Like [`start_owner_thread`], but after running `handler` the owner drops
    /// the socket without writing a response — the FIX-169 "write committed,
    /// reply lost" shape (client sees `ResponseLost` / connection reset).
    pub fn start_owner_thread_drop_response(
        config: SidecarConfig,
        handler: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Result<OwnerEndpoint, String> {
        start_owner_thread_with(config, handler, true)
    }

    fn start_owner_thread_with(
        config: SidecarConfig,
        handler: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
        drop_response: bool,
    ) -> Result<OwnerEndpoint, String> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|e| format!("failed to bind test MCP owner socket: {e}"))?;
        let endpoint = OwnerEndpoint {
            addr: listener
                .local_addr()
                .map_err(|e| format!("failed to read test MCP owner address: {e}"))?,
            token: new_token(),
            pid: std::process::id(),
            fingerprint: current_build_fingerprint(&config),
        };
        let token = endpoint.token.clone();
        let handler = Arc::new(handler);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let handler = handler.clone();
                if drop_response {
                    let _ = handle_owner_stream_drop_response(&config, &token, stream, move |line| {
                        handler(line)
                    });
                } else {
                    let _ = handle_owner_stream(&config, &token, stream, move |line| handler(line));
                }
            }
        });
        Ok(endpoint)
    }

    /// Run the handler (so a mutation can commit) then close without a reply.
    fn handle_owner_stream_drop_response(
        config: &SidecarConfig,
        token: &str,
        stream: TcpStream,
        handler: impl Fn(&str) -> Option<String>,
    ) -> Result<(), String> {
        let mut reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| format!("failed to clone MCP owner stream: {e}"))?,
        );
        let mut raw = String::new();
        reader
            .read_line(&mut raw)
            .map_err(|e| format!("failed to read MCP owner request: {e}"))?;
        let request: OwnerRequest = serde_json::from_str(raw.trim_end())
            .map_err(|e| format!("malformed MCP owner request: {e}"))?;
        if request.token == token && super::line_method(&request.line).as_deref() == Some("owner/shutdown")
        {
            // Still honor shutdown; tests that drop replies are about tool calls.
            let _ = config;
            return Ok(());
        }
        if request.token == token {
            let _ = handler(&request.line);
        }
        // Intentionally drop `stream` with no response bytes.
        drop(stream);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &Path) -> SidecarConfig {
        SidecarConfig::new("testapp", dir, dir.join("cache")).app_version("1.2.3")
    }

    #[test]
    fn idle_poll_interval_is_clamped() {
        // A 10-minute idle timeout wakes ~1/sec (capped), a short test timeout
        // wakes promptly (floored) — responsive without busy-spinning.
        assert_eq!(
            idle_poll_interval(Duration::from_secs(600)),
            Duration::from_secs(1)
        );
        assert_eq!(
            idle_poll_interval(Duration::from_millis(1500)),
            Duration::from_millis(75)
        );
        assert_eq!(
            idle_poll_interval(Duration::from_millis(100)),
            Duration::from_millis(50)
        );
    }

    #[test]
    fn effective_idle_timeout_reads_env_override_else_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path())
            .idle_timeout(Duration::from_secs(600))
            .idle_timeout_env("TURNKEY_TEST_IDLE_TIMEOUT_MS_A");
        assert_eq!(cfg.effective_idle_timeout(), Duration::from_secs(600));
        std::env::set_var("TURNKEY_TEST_IDLE_TIMEOUT_MS_A", "250");
        assert_eq!(cfg.effective_idle_timeout(), Duration::from_millis(250));
        std::env::remove_var("TURNKEY_TEST_IDLE_TIMEOUT_MS_A");
    }

    #[test]
    fn fingerprint_folds_in_the_app_version() {
        let dir = tempfile::tempdir().unwrap();
        let a = current_build_fingerprint(&config(dir.path()));
        let b = current_build_fingerprint(&config(dir.path()).app_version("9.9.9"));
        assert!(a.starts_with("1.2.3+"));
        assert!(b.starts_with("9.9.9+"));
        assert_ne!(a, b, "an app version bump must retire a stale owner");
    }

    #[test]
    fn owner_lock_is_exclusive_within_a_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let first = OwnerLock::try_acquire(&cfg).unwrap();
        assert!(first.is_some(), "first acquire wins the lock");
        let second = OwnerLock::try_acquire(&cfg).unwrap();
        assert!(
            second.is_none(),
            "a second owner must be refused while the first lives"
        );
        drop(first);
        let third = OwnerLock::try_acquire(&cfg).unwrap();
        assert!(third.is_some(), "a released lock is takeable again");
    }

    #[test]
    fn recover_owner_refuses_while_the_tried_pid_is_alive() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        // No registration on disk; the tried endpoint is unreachable but its
        // pid (ours) is alive → fail closed, never a rival writer.
        let tried = test_support::unreachable_endpoint(&cfg);
        match recover_owner(&cfg, &tried) {
            OwnerRecovery::LiveButUnreachable => {}
            _ => panic!("a live-but-unreachable owner must refuse recovery"),
        }
    }

    #[test]
    fn recover_owner_prefers_a_live_registered_owner() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let live = test_support::start_owner_thread(cfg.clone(), |_line| {
            Some(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#.to_string())
        })
        .unwrap();
        test_support::write_endpoint(&cfg, &live);
        let tried = test_support::unreachable_endpoint(&cfg);
        match recover_owner(&cfg, &tried) {
            OwnerRecovery::Reelected(endpoint) => assert_eq!(endpoint.addr, live.addr),
            _ => panic!("a live registered owner must be re-attached to"),
        }
    }

    #[test]
    fn owner_stream_rejects_a_bad_token() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let owner = test_support::start_owner_thread(cfg, |_line| {
            Some(r#"{"jsonrpc":"2.0","id":0,"result":{}}"#.to_string())
        })
        .unwrap();
        let intruder = OwnerEndpoint {
            token: "wrong-token".to_string(),
            ..owner.clone()
        };
        let response = send_line(&intruder, r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#)
            .unwrap()
            .unwrap();
        assert!(
            response.contains("Invalid resident MCP owner token"),
            "unexpected response: {response}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn owner_lock_excludes_another_process_and_releases_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let path = cfg.lock_path();

        let held = OwnerLock::try_acquire(&cfg)
            .unwrap()
            .expect("first acquire must win");
        // An external process trying the same lock non-blocking must fail while we hold it.
        let busy = Command::new("flock")
            .args(["-n", path.to_str().unwrap(), "-c", "true"])
            .status()
            .expect("run flock(1)");
        assert!(
            !busy.success(),
            "another process must be refused the lock while we hold it — no rival writer"
        );

        drop(held);
        // After we release (the crash/exit analogue), another process can take it.
        let free = Command::new("flock")
            .args(["-n", path.to_str().unwrap(), "-c", "true"])
            .status()
            .expect("run flock(1)");
        assert!(
            free.success(),
            "after the holder releases, the lock is available again (crash-safe re-election)"
        );
    }

    // The lock file lives under the cache dir and acquiring it creates that dir
    // when absent, so a brand-new workspace elects an owner without a
    // pre-created cache.
    #[test]
    fn acquire_creates_the_cache_dir_and_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let _lock = OwnerLock::try_acquire(&cfg).unwrap().expect("acquire");
        assert!(
            cfg.lock_path().exists(),
            "the lock file must be created under the cache dir"
        );
    }

    // The crux of post-reinstall resolution — Linux marks an unlinked
    // (replaced) exe path with a trailing " (deleted)"; stripping it yields the
    // real path the replacement now occupies.
    #[test]
    fn strip_deleted_marker_recovers_the_real_path() {
        assert_eq!(
            strip_deleted_marker(Path::new("/home/u/.cargo/bin/myapp (deleted)")),
            Some(PathBuf::from("/home/u/.cargo/bin/myapp")),
        );
        // A live path has no marker to strip.
        assert_eq!(
            strip_deleted_marker(Path::new("/home/u/.cargo/bin/myapp")),
            None
        );
    }

    // In the common case current_exe() exists, so resolution returns a real,
    // spawnable path.
    #[test]
    fn resolve_owner_exe_returns_an_existing_path() {
        let exe = resolve_owner_exe("myapp").expect("must resolve an owner exe");
        assert!(
            exe.exists(),
            "resolved owner exe must exist on disk: {}",
            exe.display()
        );
    }

    // The fingerprint is deterministic for one build, so two clients of the
    // same installed binary agree and do not retire each other in a loop.
    #[test]
    fn build_fingerprint_is_stable_within_a_build() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            current_build_fingerprint(&config(dir.path())),
            current_build_fingerprint(&config(dir.path()))
        );
    }

    // Control-plane routing: the shutdown method is recognized; noise is not.
    #[test]
    fn line_method_extracts_the_control_method() {
        assert_eq!(
            line_method(r#"{"jsonrpc":"2.0","id":0,"method":"owner/shutdown"}"#).as_deref(),
            Some("owner/shutdown")
        );
        assert_eq!(line_method("not json").as_deref(), None);
        assert_eq!(line_method(r#"{"id":0}"#).as_deref(), None);
    }

    // The upgrade path must also retire an owner registered before
    // fingerprinting existed: its registration has no fingerprint field, which
    // deserializes to empty and therefore mismatches any real build fingerprint
    // — so the first newer client retires it and elects a fresh owner.
    #[test]
    fn legacy_registration_without_fingerprint_mismatches_and_is_retired() {
        let dir = tempfile::tempdir().unwrap();
        let json = r#"{"addr":"127.0.0.1:5000","token":"t","pid":42}"#;
        let ep: OwnerEndpoint = serde_json::from_str(json).expect("legacy json deserializes");
        assert!(
            ep.fingerprint.is_empty(),
            "a legacy registration carries no fingerprint"
        );
        assert_ne!(
            ep.fingerprint,
            current_build_fingerprint(&config(dir.path())),
            "empty fingerprint mismatches the current build -> the stale owner is retired"
        );
    }

    // A current registration round-trips the fingerprint, so a same-build
    // client attaches without a spurious retire.
    #[test]
    fn current_registration_round_trips_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path());
        let ep = OwnerEndpoint {
            addr: "127.0.0.1:5001".parse().unwrap(),
            token: "t".to_string(),
            pid: std::process::id(),
            fingerprint: current_build_fingerprint(&cfg),
        };
        write_endpoint(&cfg, &ep).unwrap();
        let read = read_endpoint(&cfg).expect("registration read back");
        assert_eq!(read.fingerprint, current_build_fingerprint(&cfg));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_exited_unreaped_child_zombie_reads_dead() {
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn short-lived helper");
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !process_is_zombie(pid) {
            assert!(Instant::now() < deadline, "helper never became a zombie");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !process_is_alive(pid),
            "an unreaped zombie must read as dead so owner recovery can re-elect"
        );
        child.wait().expect("reap helper");
    }
}
