use ads_monitor::collector::{collect_all, demo_snapshot};
use ads_monitor::config::AppConfig;
use ads_monitor::http::build_http_client;
use ads_monitor::models::PlatformId;
use ads_monitor::render::render_html;
use ads_monitor::server::{build_router, ServerState};
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "ads-monitor",
    version,
    about = "Unified multi-platform Ads & Custom Audience Monitoring dashboard (OpenAI Ads, Meta/Facebook Ads, Google Ads, TikTok Ads, LINE OA, LINE Ads)"
)]
struct Cli {
    /// Output HTML file path when not running with --serve or --json
    #[arg(long, default_value = "dashboard.html")]
    out: PathBuf,

    /// Run a live local HTTP server that re-fetches from all configured APIs on reload
    #[arg(long)]
    serve: bool,

    /// Port to bind when using --serve
    #[arg(long, default_value_t = 8000)]
    port: u16,

    /// Host interface to bind when using --serve
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Don't automatically open the browser
    #[arg(long)]
    no_open: bool,

    /// Output raw normalized JSON snapshot to stdout instead of HTML
    #[arg(long)]
    json: bool,

    /// Comma-separated list of platforms to monitor (openai,facebook,google,tiktok,lineoa,lineads)
    #[arg(long, value_delimiter = ',')]
    platforms: Vec<PlatformId>,

    /// Lookback window in days for spend/performance metrics (default: 30)
    #[arg(long)]
    days: Option<u32>,

    /// Max concurrent sub-requests per platform (default: 8)
    #[arg(long)]
    workers: Option<usize>,

    /// HTTP request timeout in seconds (default: 30)
    #[arg(long)]
    timeout: Option<u64>,

    /// SQLite database path for snapshot caching & history (default: data/ads-monitor.db; "none" for in-memory)
    #[arg(long)]
    db_path: Option<String>,

    /// Auto-refresh interval in minutes in --serve mode (default: 30; 0 disables auto-refresh)
    #[arg(long)]
    refresh_interval: Option<u64>,

    /// Optional cron expression for --serve auto-refresh (e.g. "*/30 * * * *"); overrides --refresh-interval
    #[arg(long)]
    refresh_cron: Option<String>,

    /// Path to .env file to load (defaults to .env in current working directory if present)
    #[arg(long)]
    env_file: Option<PathBuf>,

    /// Run with built-in realistic demo data across all 6 platforms (no credentials required)
    #[arg(long)]
    demo: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let cli = Cli::parse();

    if let Some(env_path) = &cli.env_file {
        dotenvy::from_path(env_path)?;
    } else {
        let _ = dotenvy::dotenv();
    }

    let mut config = AppConfig::from_env(cli.days, cli.workers, cli.timeout, cli.platforms);
    if let Some(raw_db) = cli.db_path {
        let trimmed = raw_db.trim();
        config.db_path = if matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "" | "none" | "off" | "disabled" | "memory" | ":memory:"
        ) {
            None
        } else {
            Some(trimmed.to_string())
        };
    }
    if let Some(mins) = cli.refresh_interval {
        config.refresh_interval_mins = mins;
    }
    if let Some(cron_expr) = cli.refresh_cron {
        let trimmed = cron_expr.trim();
        config.refresh_cron = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
    }
    let client = build_http_client(config.timeout_secs)?;

    if !cli.demo && !config.has_any_configured() {
        eprintln!(
            "Warning: No platform credentials detected in environment.\n\
             Copy .env.example to .env and configure one or more platforms, or run with --demo to preview."
        );
    }

    if cli.serve {
        let addr: SocketAddr = format!("{}:{}", cli.host, cli.port).parse()?;
        let schedule_desc = if let Some(ref cron_expr) = config.refresh_cron {
            format!("cron '{cron_expr}'")
        } else if config.refresh_interval_mins > 0 {
            format!("every {}m", config.refresh_interval_mins)
        } else {
            "manual only".to_string()
        };
        let state = ServerState::new(client, config, cli.demo);
        let db_label = state.db_path_label().to_string();
        let _bg = state.spawn_background_refresh();
        let router = build_router(state);
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let url = format!("http://{}:{}", cli.host, cli.port);
        println!(
            "Serving Ads Platform Monitor at {url} (Ctrl+C to stop).\n\
             Cache: {db_label}  |  Auto-refresh: {schedule_desc}\n\
             Endpoints: {url}/  |  {url}/api/snapshot  |  POST {url}/api/refresh  |  {url}/api/history  |  {url}/api/health"
        );
        if !cli.no_open {
            let _ = open::that(&url);
        }
        axum::serve(listener, router).await?;
        return Ok(());
    }

    let snapshot = if cli.demo {
        demo_snapshot(config.lookback_days, false)
    } else {
        collect_all(&client, &config, false).await
    };

    if cli.json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }

    let html = render_html(&snapshot)?;
    std::fs::write(&cli.out, html)?;
    let abs_out = std::fs::canonicalize(&cli.out).unwrap_or(cli.out.clone());
    println!(
        "Wrote {} — {}/{} platforms healthy, {} audiences, {} operations, {} campaigns",
        abs_out.display(),
        snapshot.summary.healthy_platforms,
        snapshot.summary.configured_platforms,
        snapshot.summary.total_audiences,
        snapshot.summary.total_operations,
        snapshot.summary.total_campaigns,
    );

    if !cli.no_open {
        let _ = open::that(&abs_out);
    }

    Ok(())
}
