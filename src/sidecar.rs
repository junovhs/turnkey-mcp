//! Resident owner / sidecar runtime.
//!
//! Copy-first extraction source: `origin/ishoo/src/mcp/transport.rs`.
//! This module keeps the core Ishoo owner pattern: loopback endpoint, tokened
//! owner requests, singleton lock, stale-owner retirement, build fingerprinting,
//! endpoint reassertion, and dead-owner recovery.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OwnerEndpoint {
    pub addr: SocketAddr,
    pub token: String,
    pub pid: u32,
    pub fingerprint: String,
}

#[derive(Clone, Debug)]
pub struct SidecarConfig {
    pub app_name: String,
    pub workspace_root: PathBuf,
    pub cache_dir: PathBuf,
    pub owner_args: Vec<String>,
    pub owner_bin_name: String,
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
            workspace_root: workspace_root.into(),
            cache_dir: cache_dir.into(),
            owner_args: vec!["mcp-owner".to_string()],
        }
    }

    pub fn owner_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.owner_args = args.into_iter().map(Into::into).collect();
        self
    }

    pub fn owner_bin_name(mut self, name: impl Into<String>) -> Self {
        self.owner_bin_name = name.into();
        self
    }

    pub fn endpoint_path(&self) -> PathBuf {
        self.cache_dir.join("mcp-owner.json")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.cache_dir.join("mcp-owner.lock")
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

pub enum OwnerRecovery {
    Reelected(OwnerEndpoint),
    LiveButUnreachable,
    Down(String),
}

/// Start or attach to the resident owner for this workspace.
pub fn ensure_owner_process(config: &SidecarConfig) -> Result<OwnerEndpoint, String> {
    if let Some(endpoint) = read_endpoint(config) {
        if send_line(&endpoint, r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#).is_ok() {
            if endpoint.fingerprint == current_build_fingerprint(&config.owner_bin_name) {
                return Ok(endpoint);
            }
            retire_stale_owner(&endpoint);
        }
    }

    let _ = fs::remove_file(config.endpoint_path());
    let exe = resolve_owner_exe(&config.owner_bin_name)?;
    Command::new(exe)
        .args(&config.owner_args)
        .current_dir(&config.workspace_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to spawn resident MCP owner: {e}"))?;

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(endpoint) = read_endpoint(config).and_then(|endpoint| {
            send_line(&endpoint, r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#)
                .ok()
                .map(|_| endpoint)
        }) {
            return Ok(endpoint);
        }
        if std::time::Instant::now() >= deadline {
            return Err("resident MCP owner did not publish a usable endpoint".to_string());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Recover after an owner became unreachable mid-session.
pub fn recover_owner(config: &SidecarConfig, tried: &OwnerEndpoint) -> OwnerRecovery {
    if let Some(current) = read_endpoint(config) {
        if send_line(&current, r#"{"jsonrpc":"2.0","id":0,"method":"ping"}"#).is_ok() {
            return OwnerRecovery::Reelected(current);
        }
    }
    if process_is_alive(tried.pid) {
        return OwnerRecovery::LiveButUnreachable;
    }
    let _ = fs::remove_file(config.endpoint_path());
    match ensure_owner_process(config) {
        Ok(endpoint) => OwnerRecovery::Reelected(endpoint),
        Err(error) => OwnerRecovery::Down(error),
    }
}

/// Run the owner process. Apps usually call this from their hidden `mcp-owner`
/// subcommand and pass `server.handle_line` as the handler.
pub fn run_owner_server(
    config: SidecarConfig,
    handler: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
) -> Result<(), String> {
    let _owner_lock = match OwnerLock::try_acquire(&config)? {
        Some(lock) => lock,
        None => return Ok(()),
    };

    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("failed to bind resident MCP owner socket: {e}"))?;
    let endpoint = OwnerEndpoint {
        addr: listener
            .local_addr()
            .map_err(|e| format!("failed to read resident MCP owner address: {e}"))?,
        token: new_token(),
        pid: std::process::id(),
        fingerprint: current_build_fingerprint(&config.owner_bin_name),
    };
    write_endpoint(&config, &endpoint)?;
    spawn_owner_watchdog(config.clone(), endpoint.clone());

    let handler = std::sync::Arc::new(handler);
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let token = endpoint.token.clone();
                let handler = handler.clone();
                thread::spawn(move || {
                    let _ = handle_owner_stream(&token, stream, move |line| handler(line));
                });
            }
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
    Ok(())
}

pub fn send_line(endpoint: &OwnerEndpoint, line: &str) -> Result<Option<String>, String> {
    let mut stream = TcpStream::connect_timeout(&endpoint.addr, Duration::from_secs(1))
        .map_err(|e| format!("failed to connect to resident MCP owner: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| format!("failed to set MCP owner read timeout: {e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| format!("failed to set MCP owner write timeout: {e}"))?;
    let request = OwnerRequest {
        token: endpoint.token.clone(),
        line: line.to_string(),
    };
    serde_json::to_writer(&mut stream, &request)
        .map_err(|e| format!("failed to encode MCP owner request: {e}"))?;
    stream
        .write_all(b"\n")
        .map_err(|e| format!("failed to write MCP owner request: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("failed to flush MCP owner request: {e}"))?;

    let mut raw = String::new();
    BufReader::new(stream)
        .read_line(&mut raw)
        .map_err(|e| format!("failed to read MCP owner response: {e}"))?;
    let response: OwnerResponse = serde_json::from_str(raw.trim_end())
        .map_err(|e| format!("malformed MCP owner response: {e}"))?;
    Ok(response.response)
}

fn handle_owner_stream(
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

    let response = if request.token == token {
        if line_method(&request.line).as_deref() == Some("owner/shutdown") {
            let ack = Some(r#"{"jsonrpc":"2.0","result":{"status":"shutting_down"}}"#.to_string());
            let mut writer = stream;
            serde_json::to_writer(&mut writer, &OwnerResponse { response: ack })
                .map_err(|e| format!("failed to encode MCP owner response: {e}"))?;
            writer
                .write_all(b"\n")
                .map_err(|e| format!("failed to write MCP owner response: {e}"))?;
            let _ = writer.flush();
            std::process::exit(0);
        }
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

fn spawn_owner_watchdog(config: SidecarConfig, endpoint: OwnerEndpoint) {
    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(1));
        if !config.workspace_root.exists() {
            std::process::exit(0);
        }
        let ours = read_endpoint(&config).is_some_and(|cur| cur.pid == endpoint.pid);
        if !ours {
            let _ = write_endpoint(&config, &endpoint);
        }
    });
}

struct OwnerLock {
    _file: fs::File,
}

impl OwnerLock {
    fn try_acquire(config: &SidecarConfig) -> Result<Option<Self>, String> {
        let path = config.lock_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("failed to create owner lock dir: {e}"))?;
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

#[cfg(unix)]
fn try_lock_exclusive_nonblocking(file: &fs::File) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(code) if code == libc::EWOULDBLOCK => Ok(false),
        _ => Err(err),
    }
}

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

fn retire_stale_owner(endpoint: &OwnerEndpoint) {
    let _ = send_line(endpoint, r#"{"jsonrpc":"2.0","id":0,"method":"owner/shutdown"}"#);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if !process_is_alive(endpoint.pid) {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
}

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
        .map(|n| n.to_string_lossy().trim_end_matches(" (deleted)").to_string())
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
        "failed to locate an executable to spawn the resident MCP owner: current_exe is unavailable/deleted and '{name}' was not found on PATH"
    ))
}

fn strip_deleted_marker(exe: &Path) -> Option<PathBuf> {
    exe.to_str()?.strip_suffix(" (deleted)").map(PathBuf::from)
}

fn current_build_fingerprint(owner_bin_name: &str) -> String {
    let version = env!("CARGO_PKG_VERSION");
    let exe_sig = resolve_owner_exe(owner_bin_name)
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
    format!("{version}+{exe_sig}")
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    matches!(std::io::Error::last_os_error().raw_os_error(), Some(code) if code == libc::EPERM)
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        CloseHandle(handle);
        true
    }
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_pid: u32) -> bool {
    false
}
