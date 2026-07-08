//! In-process stdout capture for print-first CLIs adopting MCP.
//!
//! Mined from semmap's `src/mcp/capture.rs`. Many CLI apps have core fns that
//! print their result to stdout rather than returning a typed value. To expose
//! them as MCP tools without rewriting every command, [`capture_stdout`] runs
//! the command with the process-global stdout (fd 1) temporarily redirected
//! into a pipe, then returns everything it wrote as a `String`. This calls the
//! exact same code path the CLI does — it does not shell out.
//!
//! IMPORTANT: handlers built on this capture are process-globally serialized by
//! [`capture_stdout`]'s internal lock, and the redirect must never overlap a
//! response write. Register such tools as they are (the registry does not care),
//! but keep the transport serial for them — e.g. run them behind a server whose
//! reads are cheap, or accept that concurrent reads will queue on the lock.
//!
//! [`capture_stdout`] also runs the command with the process working directory
//! set to the call's `cwd` (the tool's `root`), so a relative path argument
//! resolves exactly as `cd <root> && semmap <cmd>` would — and a command that
//! echoes the path it was given prints the same relative path the CLI prints.
//!
//! Both the stdout fd and the working directory are process-global, so calls
//! are serialized by the internal capture lock.

use std::io::Write;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

static CAPTURE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn capture_lock() -> &'static Mutex<()> {
    CAPTURE_LOCK.get_or_init(|| Mutex::new(()))
}

/// Run the command closure, converting a panic into a command error instead of
/// unwinding out of the capture. This is essential: a panic that escaped here
/// would skip the stdout-fd restore below, leaving the process-global stdout
/// pointed at a dead pipe and corrupting every later response.
fn run_caught<F>(f: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String>,
{
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|payload| {
        Err(format!(
            "command panicked: {}",
            crate::types::panic_message(payload.as_ref())
        ))
    })
}

/// Run `f` with stdout captured and the working directory set to `cwd`,
/// returning `(captured_text, command_result)`.
///
/// The outer `Result` reports a failure of the capture machinery itself (chdir,
/// pipe setup, fd duplication, reader thread); the inner `Result<(), String>` is
/// the command's own outcome. Holds the global capture lock for the whole call,
/// so the stdout redirect and the `cwd` change never overlap another capture,
/// and restores the working directory afterwards even when the capture errors.
pub fn capture_stdout<F>(cwd: &Path, f: F) -> Result<(String, Result<(), String>), String>
where
    F: FnOnce() -> Result<(), String>,
{
    let _guard = capture_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let saved_cwd =
        std::env::current_dir().map_err(|e| format!("could not read current dir: {e}"))?;
    if let Err(e) = std::env::set_current_dir(cwd) {
        return Err(format!("could not enter root {}: {e}", cwd.display()));
    }

    // Not `?`: the working directory must be restored even if the capture fails.
    let result = capture_inner(f);
    let _ = std::env::set_current_dir(&saved_cwd);
    result
}

/// The platform stdout-capture core. Assumes the global capture lock is held by
/// the caller ([`capture_stdout`]) and does not touch the working directory.
#[cfg(unix)]
fn capture_inner<F>(f: F) -> Result<(String, Result<(), String>), String>
where
    F: FnOnce() -> Result<(), String>,
{
    use std::fs::File;
    use std::io::Read;
    use std::os::unix::io::FromRawFd;
    use std::thread;

    // Flush anything already buffered so it lands on the real stdout, not the pipe.
    let _ = std::io::stdout().flush();

    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err("pipe() failed".to_string());
    }
    let [read_fd, write_fd] = fds;

    // Save the real stdout so we can restore it afterwards.
    let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };
    if saved < 0 {
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
        return Err("dup(stdout) failed".to_string());
    }

    // Point stdout at the pipe's write end.
    if unsafe { libc::dup2(write_fd, libc::STDOUT_FILENO) } < 0 {
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
            libc::close(saved);
        }
        return Err("dup2(pipe -> stdout) failed".to_string());
    }
    // stdout now holds the only remaining write reference to the pipe; drop ours
    // so restoring stdout later yields EOF for the reader.
    unsafe {
        libc::close(write_fd);
    }

    // Drain the pipe on a thread so a command that writes more than the pipe
    // buffer (~64KB: generate, cat --source, inspect) can't deadlock.
    let reader = thread::spawn(move || {
        let mut file = unsafe { File::from_raw_fd(read_fd) };
        let mut buf = Vec::new();
        let _ = file.read_to_end(&mut buf);
        buf
    });

    let result = run_caught(f);
    let _ = std::io::stdout().flush();

    // Restore the real stdout. This drops the last write reference to the pipe,
    // so the reader thread observes EOF and returns.
    unsafe {
        libc::dup2(saved, libc::STDOUT_FILENO);
        libc::close(saved);
    }

    let bytes = reader
        .join()
        .map_err(|_| "stdout capture reader thread panicked".to_string())?;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), result))
}

/// The platform stdout-capture core for Windows.
///
/// Rust's stdout on Windows writes through the console HANDLE returned by
/// `GetStdHandle(STD_OUTPUT_HANDLE)`, *not* through a C-runtime fd, so the unix
/// `dup2` trick does not apply. Instead we create an anonymous pipe, point the
/// process's stdout handle at the pipe's write end via `SetStdHandle`, run the
/// command, then restore the original handle and read the pipe. A draining
/// thread prevents a deadlock when a command writes more than the pipe buffer.
/// Assumes the global capture lock is held by the caller.
#[cfg(windows)]
fn capture_inner<F>(f: F) -> Result<(String, Result<(), String>), String>
where
    F: FnOnce() -> Result<(), String>,
{
    use std::io::Read;
    use std::os::windows::io::FromRawHandle;
    use std::thread;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_OUTPUT_HANDLE};
    use windows_sys::Win32::System::Pipes::CreatePipe;

    let _ = std::io::stdout().flush();

    let mut read_handle: HANDLE = INVALID_HANDLE_VALUE;
    let mut write_handle: HANDLE = INVALID_HANDLE_VALUE;
    // SAFETY: FFI; null security attributes and default buffer size.
    if unsafe { CreatePipe(&mut read_handle, &mut write_handle, std::ptr::null(), 0) } == 0 {
        return Err("CreatePipe() failed".to_string());
    }

    // SAFETY: FFI read of the current stdout handle.
    let saved = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };

    // SAFETY: redirect the process stdout handle to the pipe's write end.
    if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, write_handle) } == 0 {
        unsafe {
            CloseHandle(read_handle);
            CloseHandle(write_handle);
        }
        return Err("SetStdHandle(pipe) failed".to_string());
    }

    // A raw `HANDLE` (*mut c_void) is not `Send`, so move it across the thread
    // boundary as an integer and rebuild the pointer inside the thread — the
    // same shape as the unix path, which moves an `i32` fd.
    let read_addr = read_handle as isize;
    let reader = thread::spawn(move || {
        let read_raw = read_addr as *mut std::ffi::c_void;
        // SAFETY: we own read_handle and hand it to a File that closes it on drop.
        let mut file = unsafe { std::fs::File::from_raw_handle(read_raw) };
        let mut buf = Vec::new();
        let _ = file.read_to_end(&mut buf);
        buf
    });

    let result = run_caught(f);
    let _ = std::io::stdout().flush();

    // Restore the original stdout, then close our write end so the reader sees EOF.
    // SAFETY: FFI; `saved` is the handle we read above.
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, saved);
        CloseHandle(write_handle);
    }

    let bytes = reader
        .join()
        .map_err(|_| "stdout capture reader thread panicked".to_string())?;
    Ok((String::from_utf8_lossy(&bytes).into_owned(), result))
}

/// Capture is implemented for unix and windows. Any other target reports the
/// platform as unsupported rather than silently returning empty text.
#[cfg(not(any(unix, windows)))]
fn capture_inner<F>(_f: F) -> Result<(String, Result<(), String>), String>
where
    F: FnOnce() -> Result<(), String>,
{
    Err("mcp-product-infra stdout capture is not supported on this platform".to_string())
}
