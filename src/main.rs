mod assets;
mod auth;
mod config;
mod discord;
mod realtime;
mod routes;
mod sfu;
mod state;
mod ws;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

/// Discord Activity screen sharing — single self-hosted binary.
#[derive(Parser)]
#[command(name = "nao-me-prenda-janja", version)]
struct Cli {
    /// Path to the config file (created interactively on first run).
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nao_me_prenda_janja=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let cfg = config::load_or_setup(&cli.config)?;
    let port = cfg.server.port;
    let http = reqwest::Client::new();
    let sfu = match cfg.rtc.backend {
        config::RtcBackend::Builtin => Some(sfu::Sfu::new(&cfg.rtc.builtin, &http).await?),
        config::RtcBackend::Cloudflare => None,
    };
    let state = Arc::new(state::AppState::new(cfg, http, sfu));
    let app = routes::router(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on http://{addr} — point your reverse proxy here");
    axum::serve(listener, app).await?;
    Ok(())
}
