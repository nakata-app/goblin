//! Headless MCP server binary. Loads the same `~/.goblin/config.toml`
//! that the desktop app uses, builds the full ToolRegistry, and serves
//! Goblin's tool surface over stdio JSON-RPC. Any MCP client (Claude
//! Code, Cursor, etc.) can point at this binary as a server and gain
//! the full tool set without launching the desktop UI.
//!
//! Usage:
//!     goblin-mcp                  # reads ~/.goblin/config.toml
//!     GOBLIN_HOME=/path goblin-mcp # honours the same env override
//!                                  # the desktop app reads
//!
//! All progress / log lines go to stderr so stdout stays a pure
//! JSON-RPC frame channel.

use goblin_app_lib::headless;

fn main() {
    if let Err(e) = headless::run_mcp_stdio() {
        eprintln!("[goblin-mcp] fatal: {}", e);
        std::process::exit(1);
    }
}
