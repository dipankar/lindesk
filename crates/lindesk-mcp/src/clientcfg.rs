//! `lindesk-mcp config --client …`: how to register this server with MCP clients.

use serde_json::json;

use crate::http::{ENDPOINT, Listen};

/// MCP clients we know how to configure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Client {
    ClaudeCode,
    ClaudeDesktop,
    Cursor,
    Generic,
}

/// The command clients should run: `lindesk-mcp` when it's the installed binary on PATH,
/// else this executable's absolute path.
pub fn command() -> String {
    let exe = std::env::current_exe().ok();
    let on_path = std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|d| {
            let c = d.join("lindesk-mcp");
            c.is_file() && exe.as_ref().is_none_or(|e| c.canonicalize().ok().as_ref() == e.canonicalize().ok().as_ref())
        })
    });
    if on_path {
        return "lindesk-mcp".into();
    }
    exe.map(|e| e.display().to_string()).unwrap_or_else(|| "lindesk-mcp".into())
}

/// The snippet for `client`. `listen` = a running HTTP endpoint (`lindesk-mcp.service`)
/// instead of spawning a stdio process.
pub fn snippet(client: Client, cmd: &str, listen: Option<&Listen>) -> String {
    let url = match listen {
        Some(Listen::Tcp(a)) => Some(format!("http://{a}{ENDPOINT}")),
        _ => None,
    };
    match (client, &url) {
        (Client::ClaudeCode, None) => format!(
            "# Register lindesk-mcp with Claude Code (stdio; Claude Code starts it on demand):\n\
             claude mcp add lindesk -- {cmd}\n\
             # or for every project of this user:\n\
             claude mcp add --scope user lindesk -- {cmd}\n"
        ),
        (Client::ClaudeCode, Some(url)) => format!(
            "# Register a running lindesk-mcp HTTP endpoint with Claude Code:\n\
             claude mcp add --transport http lindesk {url} \\\n  --header \"Authorization: Bearer $(cat \"${{XDG_RUNTIME_DIR}}/lindesk/mcp.token\")\"\n"
        ),
        (Client::ClaudeDesktop | Client::Cursor | Client::Generic, None) => {
            let file = match client {
                Client::ClaudeDesktop => "~/.config/Claude/claude_desktop_config.json",
                Client::Cursor => "~/.cursor/mcp.json (or .cursor/mcp.json in a project)",
                _ => "your MCP client's server list",
            };
            let v = json!({ "mcpServers": { "lindesk": { "command": cmd, "args": [] } } });
            format!("// Add to {file}:\n{}\n", serde_json::to_string_pretty(&v).unwrap_or_default())
        }
        (_, Some(url)) => {
            let v = json!({ "mcpServers": { "lindesk": {
                "type": "http",
                "url": url,
                "headers": { "Authorization": "Bearer <contents of $XDG_RUNTIME_DIR/lindesk/mcp.token>" }
            } } });
            format!(
                "// Streamable HTTP endpoint (start it with: systemctl --user enable --now lindesk-mcp, or lindesk-mcp --listen {}):\n{}\n",
                listen.map(ToString::to_string).unwrap_or_default(),
                serde_json::to_string_pretty(&v).unwrap_or_default()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets() {
        let s = snippet(Client::ClaudeCode, "lindesk-mcp", None);
        assert!(s.contains("claude mcp add lindesk -- lindesk-mcp"), "{s}");
        let s = snippet(Client::ClaudeDesktop, "/usr/bin/lindesk-mcp", None);
        let json_part = s.split_once('\n').unwrap().1;
        let v: serde_json::Value = serde_json::from_str(json_part).unwrap();
        assert_eq!(v["mcpServers"]["lindesk"]["command"], "/usr/bin/lindesk-mcp");
        let tcp: Listen = "127.0.0.1:7457".parse().unwrap();
        let s = snippet(Client::ClaudeCode, "lindesk-mcp", Some(&tcp));
        assert!(s.contains("--transport http lindesk http://127.0.0.1:7457/mcp"), "{s}");
        assert!(snippet(Client::Cursor, "x", Some(&tcp)).contains("\"url\": \"http://127.0.0.1:7457/mcp\""));
    }
}
