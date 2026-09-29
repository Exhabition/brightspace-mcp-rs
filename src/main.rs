mod auth;
mod mcp;
mod snapshot;

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "brightspace-mcp",
    about = "Read-only Brightspace MCP server using Microsoft SSO"
)]
struct Args {
    /// Brightspace site URL, for example https://learn.example.edu
    #[arg(long, env = "BRIGHTSPACE_BASE_URL")]
    base_url: String,

    /// Directory for the persistent visible-browser SSO profile.
    #[arg(long, env = "BRIGHTSPACE_BROWSER_PROFILE")]
    browser_profile: Option<PathBuf>,

    /// Directory for local course snapshots.
    #[arg(long, env = "BRIGHTSPACE_SYNC_DIR")]
    sync_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let base_url = url::Url::parse(&args.base_url).context("invalid Brightspace URL")?;
    if base_url.scheme() != "https"
        && !base_url
            .host_str()
            .is_some_and(|host| host == "localhost" || host == "127.0.0.1")
    {
        bail!("Brightspace URL must use HTTPS");
    }
    if base_url.path() != "/" || base_url.query().is_some() || base_url.fragment().is_some() {
        bail!("Brightspace URL must be the site origin without a path, query, or fragment");
    }

    let profile_dir = args.browser_profile.unwrap_or_else(|| {
        dirs_next::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".brightspace-mcp-rs")
            .join("browser-profile")
    });
    let sync_dir = args.sync_dir.unwrap_or_else(|| {
        dirs_next::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".brightspace-mcp-rs")
            .join("synced-courses")
    });
    let server = mcp::BrightspaceServer::new(base_url, profile_dir, sync_dir).await?;
    let service = rmcp::ServiceExt::serve(server.clone(), rmcp::transport::stdio()).await?;
    let server_result = service.waiting().await;
    server.shutdown().await?;
    server_result?;
    Ok(())
}
