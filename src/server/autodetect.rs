//! Auto-detect launch behavior for the `herdr` command.
//!
//! When the user runs `herdr` with no subcommand:
//! 1. Check if a server is already listening on the client socket
//! 2. If no server → spawn one as a background daemon → wait for socket readiness (up to 15s)
//! 3. Attach as a client to the server

use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use tracing::info;

use super::socket_paths::client_socket_path;

/// Maximum time to wait for the server's client socket to become ready
/// after spawning the server process.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Poll interval when waiting for the server socket to appear.
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Timeout for checking the stable JSON API before attaching to the binary protocol socket.
#[cfg(test)]
const STATUS_REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Private daemon-start hint used to seed a fresh headless server from the
/// directory where the user ran `herdr`.
pub(crate) const STARTUP_CWD_ENV_VAR: &str = "HERDR_STARTUP_CWD";

// ---------------------------------------------------------------------------
// Server detection
// ---------------------------------------------------------------------------

/// Checks whether a herdr server is currently listening on the client socket.
///
/// This works by attempting to connect to the client socket. If the connection
/// succeeds, a server is running. If the socket file doesn't exist or the
/// connection is refused, no server is running. Stale sockets (from a crashed
/// server) are detected because connect returns `ConnectionRefused`
/// when nobody is listening.
#[allow(dead_code)] // Public API for external use and testing
pub fn is_server_listening() -> io::Result<bool> {
    is_server_listening_at(&client_socket_path())
}

/// Checks whether a herdr server is listening at a specific socket path.
fn is_server_listening_at(socket_path: &Path) -> io::Result<bool> {
    #[cfg(windows)]
    {
        match crate::platform::probe_local_server(socket_path) {
            Ok(()) => Ok(true),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                Ok(false)
            }
            Err(err) => Err(err),
        }
    }

    #[cfg(not(windows))]
    {
        if !socket_path.exists() {
            return Ok(false);
        }

        Ok(match crate::ipc::connect_local_stream(socket_path) {
            Ok(_) => {
                // Server is listening. Close the test connection immediately.
                // The server's handshake handler will time out on this connection
                // since we don't send a handshake, which is fine.
                true
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut
                ) =>
            {
                // Socket file exists but nobody is listening — stale socket.
                false
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                // Socket file disappeared between exists() and connect().
                false
            }
            Err(err) => {
                // Other errors (permission denied, etc.) — assume not listening.
                tracing::warn!(err = %err, "unexpected error checking server socket");
                false
            }
        })
    }
}

#[cfg(test)]
fn read_server_status() -> io::Result<Option<crate::api::RuntimeStatus>> {
    crate::api::read_runtime_status_at(&crate::api::socket_path(), STATUS_REQUEST_TIMEOUT)
}

#[cfg(windows)]
fn client_protocol_accepts_hello(socket_path: &Path) -> io::Result<bool> {
    if !socket_path.exists() {
        return Ok(false);
    }

    let mut stream = match crate::ipc::connect_local_stream(socket_path) {
        Ok(stream) => stream,
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::NotFound
                    | io::ErrorKind::TimedOut
                    | io::ErrorKind::WouldBlock
            ) =>
        {
            return Ok(false);
        }
        Err(err) => return Err(err),
    };

    let hello = crate::protocol::ClientMessage::TerminalHello {
        version: crate::protocol::PROTOCOL_VERSION,
        cols: 80,
        rows: 24,
        cell_width_px: 0,
        cell_height_px: 0,
        pixel_mouse: false,
    };

    match crate::protocol::write_message(&mut stream, &hello) {
        Ok(()) => Ok(true),
        Err(crate::protocol::FramingError::Io(err))
            if matches!(
                err.kind(),
                io::ErrorKind::ConnectionRefused
                    | io::ErrorKind::NotFound
                    | io::ErrorKind::TimedOut
                    | io::ErrorKind::WouldBlock
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::ConnectionReset
            ) =>
        {
            Ok(false)
        }
        Err(err) => Err(io::Error::other(err.to_string())),
    }
}

#[cfg(test)]
fn validate_running_server_compatibility(saved_federation: bool) -> io::Result<()> {
    let Some(status) = read_server_status()? else {
        return Err(io::Error::other(format!(
            "a herdr server is listening, but its status API is unavailable.\n\n{}\nIf that fails, stop the old server process manually.",
            crate::session::active_restart_after_update_guidance()
        )));
    };

    let capabilities = status.capabilities.as_ref();
    let endpoint_generation =
        capabilities.and_then(|capabilities| capabilities.endpoint_protocol_generation);
    let surface_interest = capabilities.is_some_and(|capabilities| capabilities.surface_interest);
    if endpoint_generation == Some(crate::protocol::endpoint::ENDPOINT_PROTOCOL_GENERATION)
        && (!saved_federation || surface_interest)
    {
        return Ok(());
    }

    let requirement = if saved_federation && !surface_interest {
        "saved SSH machines require surface lifecycle support"
    } else {
        "the stable endpoint generation is incompatible"
    };
    Err(io::Error::other(format!(
        "This session needs one final server update before Herdr can attach ({requirement}).\n\nserver: v{} endpoint generation {}\nclient: v{} endpoint generation {}\n\n{}",
        status.version.as_deref().unwrap_or("unknown"),
        endpoint_generation
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unavailable".to_string()),
        crate::build_info::version(),
        crate::protocol::endpoint::ENDPOINT_PROTOCOL_GENERATION,
        crate::session::active_restart_after_update_guidance()
    )))
}

// ---------------------------------------------------------------------------
// Server spawning
// ---------------------------------------------------------------------------

/// Spawns the herdr server as a background daemon process.
///
/// The server process is fully detached:
/// - Runs in its own session (setsid) so it survives the client exiting
/// - Stdin/stdout/stderr are redirected to /dev/null
/// - Inherits relevant environment variables (`XDG_CONFIG_HOME`, `HERDR_SESSION`,
///   socket overrides, etc.), except inherited socket overrides are cleared when
///   this CLI invocation explicitly selected a session.
///
/// Returns the PID of the spawned server process.
pub fn spawn_server_daemon() -> io::Result<u32> {
    let exe = std::env::current_exe().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("failed to determine herdr executable path: {err}"),
        )
    })?;

    info!(exe = %exe.display(), "spawning server daemon");

    let mut command = build_server_daemon_command(exe);

    let pid =
        crate::platform::launch_server_daemon_command(&mut command).map_err(|err: io::Error| {
            io::Error::new(err.kind(), format!("failed to spawn herdr server: {err}"))
        })?;
    info!(pid, "server daemon spawned");

    Ok(pid)
}

fn build_server_daemon_command(exe: PathBuf) -> Command {
    let mut command = Command::new(&exe);
    command
        .arg("server")
        // Redirect stdio to /dev/null
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    crate::platform::detach_server_daemon_command(&mut command);

    match std::env::current_dir() {
        Ok(cwd) => {
            command.env(STARTUP_CWD_ENV_VAR, cwd);
        }
        Err(_) => {
            command.env_remove(STARTUP_CWD_ENV_VAR);
        }
    }

    if crate::session::explicit_session_requested() {
        command
            .env_remove(crate::api::SOCKET_PATH_ENV_VAR)
            .env_remove("HERDR_CLIENT_SOCKET_PATH");
    }

    command
}

// ---------------------------------------------------------------------------
// Socket readiness
// ---------------------------------------------------------------------------

/// Waits for the server's client socket to become ready for connections.
///
/// Polls the socket path at regular intervals until a connection succeeds
/// or the timeout elapses. Returns an error if the server doesn't become
/// ready within the timeout.
pub fn wait_for_server_socket(socket_path: &Path, timeout: Duration) -> io::Result<()> {
    let deadline = std::time::Instant::now() + timeout;

    while std::time::Instant::now() < deadline {
        #[cfg(windows)]
        if client_protocol_accepts_hello(socket_path)? {
            info!(path = %socket_path.display(), "server client protocol ready");
            return Ok(());
        }

        #[cfg(not(windows))]
        if is_server_listening_at(socket_path)? {
            info!(path = %socket_path.display(), "server socket ready");
            return Ok(());
        }
        std::thread::sleep(SOCKET_POLL_INTERVAL);
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "server did not become ready within {}s (socket: {}). The background server may still be starting; try `herdr` again, or check {}",
            timeout.as_secs(),
            socket_path.display(),
            crate::session::data_dir().join("herdr-server.log").display()
        ),
    ))
}

// ---------------------------------------------------------------------------
// Auto-detect launch
// ---------------------------------------------------------------------------

fn temp_tui_socket_dir() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from(format!("/tmp/herdr-tui-{}", std::process::id()))
    }
    #[cfg(not(unix))]
    {
        std::env::temp_dir().join(format!("herdr-tui-{}", std::process::id()))
    }
}

fn prepare_temp_tui_socket_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

struct InprocessEnvGuard {
    prev_api_socket: Option<std::ffi::OsString>,
    prev_client_socket: Option<std::ffi::OsString>,
    prev_startup_cwd: Option<std::ffi::OsString>,
    prev_hc_remote_client: Option<std::ffi::OsString>,
    prev_remote_prefs: Option<std::ffi::OsString>,
    temp_dir: PathBuf,
    #[cfg(unix)]
    canonical_api_symlink: Option<(PathBuf, PathBuf)>,
}

impl Drop for InprocessEnvGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        remove_canonical_api_symlink(self.canonical_api_symlink.as_ref());
        match &self.prev_api_socket {
            Some(val) => std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, val),
            None => std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR),
        }
        match &self.prev_client_socket {
            Some(val) => {
                std::env::set_var(crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR, val)
            }
            None => std::env::remove_var(crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR),
        }
        match &self.prev_startup_cwd {
            Some(val) => std::env::set_var(STARTUP_CWD_ENV_VAR, val),
            None => std::env::remove_var(STARTUP_CWD_ENV_VAR),
        }
        match &self.prev_hc_remote_client {
            Some(val) => std::env::set_var(crate::hc::client::HC_REMOTE_CLIENT_ENV, val),
            None => std::env::remove_var(crate::hc::client::HC_REMOTE_CLIENT_ENV),
        }
        match &self.prev_remote_prefs {
            Some(val) => std::env::set_var(crate::remote::REMOTE_PREFERENCES_ENV_VAR, val),
            None => std::env::remove_var(crate::remote::REMOTE_PREFERENCES_ENV_VAR),
        }
        let _ = std::fs::remove_dir_all(&self.temp_dir);
    }
}

/// Performs auto-detect launch using an in-process `HeadlessServer` backed by
/// the already-installed `hc` (`herdr-core`) daemon (local or over SSH via `--remote`).
pub fn auto_detect_launch(saved_federation: bool, remote_target: Option<&str>) -> io::Result<()> {
    crate::platform::terminal_grid_size().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!("cannot attach without a usable terminal: {err}; run inside a terminal"),
        )
    })?;

    let hc_session = crate::session::active_name();
    if let Some(target) = remote_target {
        let core = crate::hc::Core::connect_remote(target, hc_session.as_deref())?;
        crate::hc::set_global_core(Some(core));
    } else {
        match crate::hc::core_bin::find(None) {
            Some(core_bin) => {
                crate::hc::core_bin::use_core(core_bin);
                match crate::hc::Core::connect(hc_session.as_deref()) {
                    Ok(core) => {
                        crate::hc::set_global_core(Some(core));
                    }
                    Err(error) => {
                        if !saved_federation {
                            return Err(error);
                        }
                        tracing::warn!(%error, "hc connection failed; keeping saved machines available");
                    }
                }
            }
            None => {
                let err = io::Error::new(
                    io::ErrorKind::NotFound,
                    "cannot find `hc` (herdr-core) on this machine; install `hc` or `herdr-app` first, or set HC_BIN",
                );
                if !saved_federation {
                    return Err(err);
                }
                tracing::warn!(%err, "hc binary not found; keeping saved machines available");
            }
        }
    }

    let temp_dir = temp_tui_socket_dir();
    prepare_temp_tui_socket_dir(&temp_dir)?;
    let api_sock = temp_dir.join("herdr.sock");
    let client_sock = temp_dir.join("herdr-client.sock");

    crate::session::clear_explicit_session_override();
    let canonical_api_sock = crate::session::api_socket_path_for(hc_session.as_deref());
    let mut _env_guard = InprocessEnvGuard {
        prev_api_socket: std::env::var_os(crate::api::SOCKET_PATH_ENV_VAR),
        prev_client_socket: std::env::var_os(
            crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
        ),
        prev_startup_cwd: std::env::var_os(STARTUP_CWD_ENV_VAR),
        prev_hc_remote_client: std::env::var_os(crate::hc::client::HC_REMOTE_CLIENT_ENV),
        prev_remote_prefs: std::env::var_os(crate::remote::REMOTE_PREFERENCES_ENV_VAR),
        temp_dir: temp_dir.clone(),
        #[cfg(unix)]
        canonical_api_symlink: None,
    };
    if let Some(target) = remote_target {
        std::env::set_var(crate::hc::client::HC_REMOTE_CLIENT_ENV, "1");
        let sess = hc_session.as_deref().unwrap_or("default");
        std::env::set_var(
            crate::remote::REMOTE_PREFERENCES_ENV_VAR,
            format!("hc:{target}:{sess}"),
        );
    }
    std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, &api_sock);
    std::env::set_var(
        crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR,
        &client_sock,
    );
    if std::env::var_os(STARTUP_CWD_ENV_VAR).is_none() {
        if let Ok(cwd) = std::env::current_dir() {
            std::env::set_var(STARTUP_CWD_ENV_VAR, cwd);
        }
    }

    let server_stop = crate::server::shutdown::ServerStop::default();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let stop_for_thread = server_stop.clone();
    let server_thread = std::thread::Builder::new()
        .name("herdr-inprocess-server".into())
        .spawn(move || {
            if let Err(err) =
                crate::server::headless::run_inprocess_server(stop_for_thread, ready_tx)
            {
                tracing::error!(err = %err, "in-process headless server exited with error");
            }
        })
        .map_err(io::Error::other)?;

    let render_notify = match ready_rx.recv_timeout(SERVER_READY_TIMEOUT) {
        Ok(Ok(notify)) => Some(notify),
        Ok(Err(err)) => {
            server_stop.request(crate::server::shutdown::ShutdownReason::HostShutdown);
            let _ = server_thread.join();
            crate::hc::set_global_core(None);
            crate::hc::reset_registry();
            return Err(err);
        }
        Err(_) => {
            server_stop.request(crate::server::shutdown::ShutdownReason::HostShutdown);
            let _ = server_thread.join();
            crate::hc::set_global_core(None);
            crate::hc::reset_registry();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for in-process TUI server startup",
            ));
        }
    };

    if let Err(err) = wait_for_server_socket(&client_sock, SERVER_READY_TIMEOUT) {
        server_stop.request(crate::server::shutdown::ShutdownReason::HostShutdown);
        if let Some(notify) = &render_notify {
            notify.notify_one();
        }
        let _ = server_thread.join();
        crate::hc::set_global_core(None);
        crate::hc::reset_registry();
        return Err(err);
    }

    #[cfg(unix)]
    if canonical_api_sock != api_sock {
        _env_guard.canonical_api_symlink =
            expose_canonical_api_symlink(&canonical_api_sock, &api_sock);
    }

    let client_result = crate::client::run_client();

    server_stop.request(crate::server::shutdown::ShutdownReason::ApiStop {
        caller: Some("in-process TUI client exit".into()),
    });
    if let Some(notify) = &render_notify {
        notify.notify_one();
    }
    let _ = server_thread.join();
    crate::hc::set_global_core(None);
    crate::hc::reset_registry();

    client_result
}

#[cfg(unix)]
fn expose_canonical_api_symlink(canonical: &Path, target: &Path) -> Option<(PathBuf, PathBuf)> {
    if is_server_listening_at(canonical).unwrap_or(false) {
        return None;
    }
    if let Some(parent) = canonical.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::symlink_metadata(canonical).is_ok() {
        let _ = std::fs::remove_file(canonical);
    }
    std::os::unix::fs::symlink(target, canonical)
        .ok()
        .map(|()| (canonical.to_path_buf(), target.to_path_buf()))
}

#[cfg(unix)]
fn remove_canonical_api_symlink(link: Option<&(PathBuf, PathBuf)>) {
    let Some((canonical, target)) = link else {
        return;
    };
    if std::fs::read_link(canonical).ok().as_ref() == Some(target) {
        let _ = std::fs::remove_file(canonical);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use interprocess::local_socket::{prelude::*, GenericNamespaced, ListenerOptions};
    use interprocess::os::windows::local_socket::ListenerOptionsExt as _;
    use interprocess::os::windows::security_descriptor::SecurityDescriptor;

    #[test]
    fn reachability_distinguishes_denied_silent_busy_and_absent_pipes() {
        let path =
            std::env::temp_dir().join(format!("herdr-startup-probe-{}.sock", std::process::id()));
        let name = path.to_string_lossy();
        let denied = ListenerOptions::new()
            .name(name.to_ns_name::<GenericNamespaced>().unwrap())
            .security_descriptor(
                SecurityDescriptor::deserialize(
                    &widestring::U16CString::from_str("D:P(A;;GA;;;SY)").unwrap(),
                )
                .unwrap(),
            )
            .create_sync()
            .unwrap();
        assert_eq!(
            is_server_listening_at(&path).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        drop(denied);
        // A real listener that never answers the status API is still present:
        // automatic startup must not replace it just because it is unresponsive.
        let silent = crate::ipc::bind_local_listener(&path).unwrap();
        assert!(is_server_listening_at(&path).unwrap());
        drop(silent);
        let busy = crate::ipc::bind_local_listener(&path).unwrap();
        let occupied = crate::ipc::connect_local_stream(&path).unwrap();
        let start_probe = || {
            let probe_path = path.clone();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = result_tx.send(is_server_listening_at(&probe_path));
            });
            result_rx
        };
        assert_eq!(
            start_probe()
                .recv_timeout(Duration::from_secs(5))
                .expect("an occupied pipe must not block startup")
                .unwrap_err()
                .kind(),
            io::ErrorKind::TimedOut
        );
        let retry = start_probe();
        assert!(matches!(
            retry.recv_timeout(Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        let accepted = busy.accept().unwrap();
        assert!(retry.recv_timeout(Duration::from_secs(5)).unwrap().unwrap());
        drop(accepted);
        drop(occupied);
        drop(busy);
        assert!(!is_server_listening_at(&path).unwrap());
        std::fs::remove_file(path).unwrap();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn unique_test_dir(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::path::PathBuf::from(format!("/tmp/ha-{name}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn is_server_listening_returns_false_for_nonexistent_path() {
        let dir = unique_test_dir("nonexistent");
        let path = dir.join("s.sock");
        assert!(!is_server_listening_at(&path).unwrap());
    }

    #[test]
    fn server_daemon_command_clears_socket_overrides_for_explicit_session() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, "/tmp/inherited.sock");
        std::env::set_var("HERDR_CLIENT_SOCKET_PATH", "/tmp/inherited-client.sock");
        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        let args = vec![
            "herdr".to_string(),
            "--session".to_string(),
            "work".to_string(),
        ];
        crate::session::configure_from_args(&args).unwrap();

        let command = build_server_daemon_command(PathBuf::from("/tmp/herdr-test"));
        let envs: Vec<_> = command.get_envs().collect();

        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new(crate::api::SOCKET_PATH_ENV_VAR) && value.is_none()
        }));
        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new("HERDR_CLIENT_SOCKET_PATH") && value.is_none()
        }));
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        std::env::remove_var("HERDR_CLIENT_SOCKET_PATH");
        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
    }

    #[test]
    fn server_daemon_command_passes_current_dir_as_startup_cwd() {
        let expected = std::env::current_dir().unwrap();
        let command = build_server_daemon_command(PathBuf::from("/tmp/herdr-test"));
        let envs: Vec<_> = command.get_envs().collect();

        assert!(envs.iter().any(|(key, value)| {
            *key == OsStr::new(STARTUP_CWD_ENV_VAR) && value == &Some(expected.as_os_str())
        }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn server_daemon_detach_creates_new_session() {
        let mut command = Command::new("sh");
        command.arg("-c").arg(
            r#"sid=$(ps -o sid= -p $$ | tr -d ' ')
test "$sid" = "$$"
"#,
        );
        crate::platform::detach_server_daemon_command(&mut command);

        let status = command.status().unwrap();
        assert!(
            status.success(),
            "detached server child should be its own session leader"
        );
    }

    #[test]
    fn is_server_listening_returns_true_for_live_socket() {
        let dir = unique_test_dir("live");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");

        let _listener = UnixListener::bind(&path).unwrap();
        assert!(is_server_listening_at(&path).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn is_server_listening_returns_false_for_stale_socket() {
        let dir = unique_test_dir("stale");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");

        // Create a socket and immediately drop the listener.
        // This leaves a stale socket file with nobody listening.
        {
            let _listener = UnixListener::bind(&path).unwrap();
        }

        // The socket file exists but nobody is listening.
        assert!(!is_server_listening_at(&path).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn is_server_listening_returns_false_when_listener_dropped() {
        let dir = unique_test_dir("dropped");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");

        // Bind and immediately drop the listener.
        drop(UnixListener::bind(&path).unwrap());

        // Socket is stale — should return false.
        assert!(!is_server_listening_at(&path).unwrap());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wait_for_server_socket_succeeds_immediately() {
        let dir = unique_test_dir("wait-ok");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");

        let _listener = UnixListener::bind(&path).unwrap();

        // Should succeed immediately (socket is already ready).
        let result = wait_for_server_socket(&path, Duration::from_millis(100));
        assert!(result.is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wait_for_server_socket_times_out() {
        let dir = unique_test_dir("wait-timeout");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");

        // No listener — should time out.
        let result = wait_for_server_socket(&path, Duration::from_millis(50));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wait_for_server_socket_succeeds_after_delay() {
        let dir = unique_test_dir("wait-delay");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");

        // Spawn a thread that will create the listener after a short delay.
        let path_clone = path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let _listener = UnixListener::bind(&path_clone).unwrap();
            // Keep the listener alive for a bit.
            std::thread::sleep(Duration::from_secs(1));
        });

        // Wait with a generous timeout — should succeed.
        let result = wait_for_server_socket(&path, Duration::from_secs(2));
        assert!(result.is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_server_status_at_reads_ping_response() {
        let dir = unique_test_dir("status");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert!(request.contains("ping"));
            stream
                .write_all(
                    b"{\"id\":\"autodetect:server:status\",\"result\":{\"type\":\"pong\",\"version\":\"0.5.5\",\"protocol\":2}}\n",
                )
                .unwrap();
            stream.flush().unwrap();
        });

        let status = crate::api::read_runtime_status_at(&path, Duration::from_millis(200))
            .unwrap()
            .unwrap();
        let _ = handle.join();
        assert_eq!(status.version.as_deref(), Some("0.5.5"));
        assert_eq!(status.protocol, Some(2));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_running_server_compatibility_fails_when_status_api_missing() {
        let _guard = env_lock().lock().unwrap();
        let dir = unique_test_dir("missing-api");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api.sock");
        std::env::set_var(crate::api::SOCKET_PATH_ENV_VAR, &path);

        let err = validate_running_server_compatibility(false).unwrap_err();

        assert!(
            err.to_string().contains("status API is unavailable"),
            "unexpected error: {err}"
        );
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn validate_running_server_compatibility_names_session_commands_for_protocol_mismatch() {
        let _guard = env_lock().lock().unwrap();
        let dir = unique_test_dir("named-protocol");
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        std::env::set_var(crate::session::SESSION_ENV_VAR, "work");
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        let path = crate::session::api_socket_path_for(Some("work"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(&path).unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            assert!(request.contains("ping"));
            let body = format!(
                "{{\"id\":\"autodetect:server:status\",\"result\":{{\"type\":\"pong\",\"version\":\"0.5.5\",\"protocol\":{}}}}}\n",
                crate::protocol::PROTOCOL_VERSION + 1
            );
            stream.write_all(body.as_bytes()).unwrap();
            stream.flush().unwrap();
        });

        let err = validate_running_server_compatibility(false).unwrap_err();
        let message = err.to_string();

        let _ = handle.join();
        assert!(
            message.contains("Stop the old server to use the new version"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("Run `herdr session stop work`"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("then run `herdr session attach work` again"),
            "unexpected error: {message}"
        );
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var(crate::session::SESSION_ENV_VAR);
        std::env::remove_var(crate::api::SOCKET_PATH_ENV_VAR);
        crate::session::clear_explicit_session_for_test();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn canonical_api_symlink_created_and_cleaned_up() {
        let dir = unique_test_dir("api-symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("inprocess.sock");
        let listener = UnixListener::bind(&target).unwrap();
        let canonical = dir.join("nested").join("herdr.sock");

        let created = expose_canonical_api_symlink(&canonical, &target);
        assert_eq!(
            created,
            Some((canonical.clone(), target.clone())),
            "should create symlink to listening socket"
        );
        assert_eq!(std::fs::read_link(&canonical).unwrap(), target);
        assert!(is_server_listening_at(&canonical).unwrap());
        assert_eq!(
            expose_canonical_api_symlink(&canonical, &target),
            None,
            "should not overwrite live canonical socket"
        );

        remove_canonical_api_symlink(created.as_ref());
        assert!(std::fs::symlink_metadata(&canonical).is_err());

        drop(listener);
        let _ = std::fs::remove_dir_all(dir);
    }
}
