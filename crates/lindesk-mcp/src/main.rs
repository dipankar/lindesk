//! `lindesk-mcp`: MCP server for lindesk (stdio by default, streamable HTTP with --listen).

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rmcp::ServiceExt;
use tracing_subscriber::EnvFilter;

use lindesk_mcp::clientcfg::{self, Client};
use lindesk_mcp::http::{self, Listen};
use lindesk_mcp::{LindeskMcp, host_backends};

#[derive(Parser, Debug)]
#[command(
    name = "lindesk-mcp",
    version = lindesk_version::LONG,
    about = "MCP server for lindesk: lets a local AI agent assess responsiveness and apply guardrailed, versioned, revertible fixes",
    long_about = "MCP server for lindesk.\n\n\
        By default it speaks MCP over stdio (register it with `claude mcp add lindesk -- lindesk-mcp`). \
        With --listen it serves streamable HTTP on a unix socket (mode 0600) or a loopback TCP port \
        (requires `Authorization: Bearer <token>`, token in $XDG_RUNTIME_DIR/lindesk/mcp.token).\n\n\
        lindesk-mcp holds no privileges: changes go to lindesk-session / lindeskd over D-Bus, which \
        enforce guardrails, polkit and versioning. Logs go to stderr only."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Serve MCP over stdio (the default).
    #[arg(long, conflicts_with = "listen")]
    stdio: bool,

    /// Serve streamable HTTP instead: `unix:/path/to.sock` or a loopback `127.0.0.1:7457`.
    #[arg(long, value_name = "ADDR", env = "LINDESK_MCP_LISTEN")]
    listen: Option<Listen>,

    /// Log filter (stderr only), e.g. `info`, `lindesk_mcp=debug`. Default: $RUST_LOG, else `warn` (stdio) / `info` (HTTP).
    #[arg(long, value_name = "FILTER")]
    log_level: Option<String>,

    /// Don't contact the daemons: read-only tools use local probing, mutating tools fail.
    #[arg(long, hide = true, env = "LINDESK_MCP_NO_DAEMON")]
    no_daemon: bool,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Print the snippet that registers lindesk-mcp with an MCP client.
    Config {
        /// The MCP client.
        #[arg(long, value_enum, default_value = "claude-code")]
        client: Client,
        /// Describe a running HTTP endpoint (e.g. 127.0.0.1:7457) instead of the stdio command.
        #[arg(long, value_name = "ADDR")]
        listen: Option<Listen>,
    },
}

fn init_logging(filter: Option<&str>, http: bool) {
    let default = if http { "info" } else { "warn" };
    let filter = filter
        .map(EnvFilter::new)
        .or_else(|| EnvFilter::try_from_default_env().ok())
        .unwrap_or_else(|| EnvFilter::new(default));
    tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).with_ansi(false).init();
}

async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending::<()>().await } } => {}
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if let Some(Cmd::Config { client, listen }) = &cli.command {
        print!("{}", clientcfg::snippet(*client, &clientcfg::command(), listen.as_ref()));
        return ExitCode::SUCCESS;
    }
    let socket_activated = std::env::var_os("LISTEN_FDS").is_some();
    let http_mode = cli.listen.is_some() || socket_activated;
    init_logging(cli.log_level.as_deref(), http_mode);

    let rt = match tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("lindesk-mcp: starting the runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let backends = host_backends(!cli.no_daemon);
    let res: anyhow::Result<()> = rt.block_on(async move {
        if http_mode {
            http::serve(cli.listen, backends, shutdown_signal()).await
        } else {
            tracing::info!("lindesk-mcp {} on stdio", env!("CARGO_PKG_VERSION"));
            let service = LindeskMcp::new(backends).serve(rmcp::transport::stdio()).await?;
            tokio::select! {
                r = service.waiting() => { r?; }
                _ = shutdown_signal() => {}
            }
            Ok(())
        }
    });
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lindesk-mcp: {e:#}");
            ExitCode::FAILURE
        }
    }
}
