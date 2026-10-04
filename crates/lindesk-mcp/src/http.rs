//! Streamable HTTP transport: a unix socket (0600, no token) or a loopback TCP port (bearer
//! token required), optionally inherited from systemd socket activation (`LISTEN_FDS`).

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;

use crate::backend::Backends;
use crate::server::LindeskMcp;

/// Path of the MCP endpoint.
pub const ENDPOINT: &str = "/mcp";

/// Where to listen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Listen {
    Unix(PathBuf),
    Tcp(SocketAddr),
}

impl fmt::Display for Listen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Listen::Unix(p) => write!(f, "unix:{}", p.display()),
            Listen::Tcp(a) => write!(f, "{a}"),
        }
    }
}

impl FromStr for Listen {
    type Err = String;

    /// `unix:/path/to.sock`, `127.0.0.1:7457`, `[::1]:7457` or `localhost:7457`. Non-loopback
    /// addresses are refused: lindesk-mcp is a local service.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(path) = s.strip_prefix("unix:") {
            if path.is_empty() {
                return Err("unix: needs a socket path, e.g. unix:$XDG_RUNTIME_DIR/lindesk/mcp.sock".into());
            }
            return Ok(Listen::Unix(PathBuf::from(path)));
        }
        let addr = if let Some(port) = s.strip_prefix("localhost:") {
            let port: u16 = port.parse().map_err(|_| format!("bad port in '{s}'"))?;
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
        } else {
            s.parse::<SocketAddr>()
                .map_err(|_| format!("'{s}' is neither unix:/path nor a host:port such as 127.0.0.1:7457"))?
        };
        if !addr.ip().is_loopback() {
            return Err(format!(
                "refusing to listen on {addr}: lindesk-mcp only binds loopback addresses (127.0.0.0/8, ::1) or unix sockets"
            ));
        }
        Ok(Listen::Tcp(addr))
    }
}

/// `$XDG_RUNTIME_DIR`, else `/run/user/<uid>`.
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        // SAFETY: getuid has no preconditions.
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
}

/// `$XDG_RUNTIME_DIR/lindesk/mcp.token`.
pub fn token_path() -> PathBuf {
    runtime_dir().join("lindesk/mcp.token")
}

/// `$XDG_RUNTIME_DIR/lindesk/mcp.sock`.
pub fn default_socket() -> PathBuf {
    runtime_dir().join("lindesk/mcp.sock")
}

fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    if !dir.exists() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    Ok(())
}

/// Read the bearer token, creating it (32 random bytes, hex, mode 0600) on first start. An
/// existing token that others can read is replaced.
pub fn load_or_create_token(path: &Path) -> io::Result<String> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    if let Ok(meta) = std::fs::metadata(path) {
        let private = meta.permissions().mode() & 0o077 == 0;
        // SAFETY: getuid has no preconditions.
        let mine = std::os::unix::fs::MetadataExt::uid(&meta) == unsafe { libc::getuid() };
        if private && mine {
            let mut s = String::new();
            std::fs::File::open(path)?.read_to_string(&mut s)?;
            let s = s.trim().to_string();
            if s.len() >= 32 {
                return Ok(s);
            }
        }
        std::fs::remove_file(path)?;
    }
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        f.write_all(token.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(token)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn require_bearer(State(token): State<Arc<String>>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .is_some_and(|t| constant_time_eq(t.trim().as_bytes(), token.as_bytes()));
    if ok {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer realm=\"lindesk-mcp\"")],
            "401 Unauthorized: send `Authorization: Bearer <token>` (the token is in $XDG_RUNTIME_DIR/lindesk/mcp.token)\n",
        )
            .into_response()
    }
}

/// The HTTP app: the MCP endpoint at [`ENDPOINT`], behind a bearer check when `token` is set.
pub fn router(backends: Backends, token: Option<String>, cancel: CancellationToken) -> Router {
    let config = StreamableHttpServerConfig::default()
        .with_cancellation_token(cancel)
        // Browsers must never reach a local control plane: reject any Origin header.
        .enforce_origin_validation();
    let service = StreamableHttpService::new(
        move || Ok(LindeskMcp::new(backends.clone())),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let app = Router::new().nest_service(ENDPOINT, service);
    match token {
        Some(t) => app.layer(middleware::from_fn_with_state(Arc::new(t), require_bearer)),
        None => app,
    }
}

/// Bind a unix socket with mode 0600 (its directory is created 0700), replacing a stale
/// socket file but never any other kind of file.
pub fn bind_unix(path: &Path) -> io::Result<tokio::net::UnixListener> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        use std::os::unix::fs::FileTypeExt;
        if !meta.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket; refusing to replace it", path.display()),
            ));
        }
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("{} is in use by another lindesk-mcp (or the socket unit)", path.display()),
            ));
        }
        std::fs::remove_file(path)?;
    }
    let l = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

/// A listener passed by systemd (`LISTEN_PID`/`LISTEN_FDS`, first fd = 3).
pub enum Inherited {
    Unix(tokio::net::UnixListener),
    Tcp(tokio::net::TcpListener),
}

/// Take the first socket-activation fd, if systemd passed one to this process.
pub fn inherited_listener() -> io::Result<Option<Inherited>> {
    use std::os::fd::FromRawFd;
    let pid_ok = std::env::var("LISTEN_PID").ok().and_then(|p| p.parse::<u32>().ok()) == Some(std::process::id());
    let n = std::env::var("LISTEN_FDS").ok().and_then(|n| n.parse::<i32>().ok()).unwrap_or(0);
    if !pid_ok || n < 1 {
        return Ok(None);
    }
    const FD: i32 = 3;
    // SAFETY: a zeroed sockaddr_storage is a valid out-buffer for getsockname.
    let mut ss: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: FD is owned by this process per the sd_listen_fds protocol; ss/len are valid.
    if unsafe { libc::getsockname(FD, (&raw mut ss).cast(), &mut len) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: FD_CLOEXEC on our own fd.
    unsafe { libc::fcntl(FD, libc::F_SETFD, libc::FD_CLOEXEC) };
    match i32::from(ss.ss_family) {
        libc::AF_UNIX => {
            // SAFETY: FD is a listening unix socket handed to us; we take ownership once.
            let l = unsafe { std::os::unix::net::UnixListener::from_raw_fd(FD) };
            l.set_nonblocking(true)?;
            Ok(Some(Inherited::Unix(tokio::net::UnixListener::from_std(l)?)))
        }
        libc::AF_INET | libc::AF_INET6 => {
            // SAFETY: as above, a listening TCP socket.
            let l = unsafe { std::net::TcpListener::from_raw_fd(FD) };
            l.set_nonblocking(true)?;
            let l = tokio::net::TcpListener::from_std(l)?;
            if !l.local_addr()?.ip().is_loopback() {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "inherited TCP socket is not loopback"));
            }
            Ok(Some(Inherited::Tcp(l)))
        }
        f => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("inherited fd 3 has unsupported family {f}"))),
    }
}

/// Serve until `shutdown` resolves.
pub async fn serve(
    listen: Option<Listen>,
    backends: Backends,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let cancel = CancellationToken::new();
    let stop = {
        let cancel = cancel.clone();
        async move {
            shutdown.await;
            cancel.cancel();
        }
    };
    let inherited = inherited_listener()?;
    // A socket systemd passed us stays systemd's: don't unlink it on exit.
    let owns_socket = inherited.is_none();
    let listener = match (inherited, listen) {
        (Some(l), _) => {
            tracing::info!("using the socket passed by systemd");
            l
        }
        (None, Some(Listen::Unix(p))) => {
            Inherited::Unix(bind_unix(&p).map_err(|e| anyhow::anyhow!("binding {}: {e}", p.display()))?)
        }
        (None, Some(Listen::Tcp(a))) => {
            Inherited::Tcp(tokio::net::TcpListener::bind(a).await.map_err(|e| anyhow::anyhow!("binding {a}: {e}"))?)
        }
        (None, None) => anyhow::bail!("no --listen address and no socket from systemd"),
    };
    match listener {
        Inherited::Unix(l) => {
            let at = l.local_addr().ok().and_then(|a| a.as_pathname().map(Path::to_path_buf));
            tracing::info!(socket = ?at, "lindesk-mcp: streamable HTTP on unix socket, endpoint {ENDPOINT}");
            let app = router(backends, None, cancel.clone());
            axum::serve(l, app).with_graceful_shutdown(stop).await?;
            if let Some(p) = at.filter(|_| owns_socket) {
                let _ = std::fs::remove_file(p);
            }
        }
        Inherited::Tcp(l) => {
            let path = token_path();
            let token = load_or_create_token(&path)
                .map_err(|e| anyhow::anyhow!("creating the bearer token {}: {e}", path.display()))?;
            let addr = l.local_addr()?;
            tracing::info!(%addr, token = %path.display(), "lindesk-mcp: streamable HTTP on http://{addr}{ENDPOINT} (bearer token required)");
            eprintln!("lindesk-mcp listening on http://{addr}{ENDPOINT} (token: {})", path.display());
            let app = router(backends, Some(token), cancel.clone());
            axum::serve(l, app).with_graceful_shutdown(stop).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_listen_addresses() {
        assert_eq!("unix:/run/x.sock".parse::<Listen>().unwrap(), Listen::Unix("/run/x.sock".into()));
        assert_eq!("127.0.0.1:7457".parse::<Listen>().unwrap(), Listen::Tcp("127.0.0.1:7457".parse().unwrap()));
        assert_eq!("localhost:7457".parse::<Listen>().unwrap(), Listen::Tcp("127.0.0.1:7457".parse().unwrap()));
        assert!("[::1]:7457".parse::<Listen>().is_ok());
        assert!("0.0.0.0:7457".parse::<Listen>().unwrap_err().contains("loopback"));
        assert!("192.168.1.5:7457".parse::<Listen>().is_err());
        assert!("unix:".parse::<Listen>().is_err());
        assert!("nonsense".parse::<Listen>().is_err());
    }

    #[test]
    fn token_is_private_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("lindesk/mcp.token");
        let t1 = load_or_create_token(&p).unwrap();
        assert_eq!(t1.len(), 64);
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(p.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(load_or_create_token(&p).unwrap(), t1);
        // A world-readable token is not trusted.
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let t2 = load_or_create_token(&p).unwrap();
        assert_ne!(t1, t2);
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
