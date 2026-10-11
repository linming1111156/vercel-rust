use std::sync::Arc;

use nzws_rust::config::Config;
use nzws_rust::{agent, app, keepalive, web};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Arc::new(Config::from_env());
    init_tracing(config.show_log);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(config))
}

async fn run(config: Arc<Config>) -> Result<(), Box<dyn std::error::Error>> {
    let state = app::AppState::new((*config).clone());

    tokio::spawn(agent::run(Arc::clone(&config)));

    let keepalive_config = Arc::clone(&config);
    let keepalive_http = state.http.clone();
    tokio::spawn(async move {
        keepalive::register(&keepalive_config, &keepalive_http).await;
    });

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.port)).await?;
    tracing::info!("Server is running on {}", config.port);
    axum::serve(listener, web::router(state)).await?;
    Ok(())
}

/// SHOW_LOG 未开启时只保留启动/错误等关键日志（对应 index.js 的日志开关）。
fn init_tracing(verbose: bool) {
    let level = if verbose { "debug" } else { "info" };
    let filter = std::env::var("RUST_LOG")
        .unwrap_or_else(|_| format!("nzws_rust={level},tower=warn,hyper=warn,tonic=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
        .with_target(false)
        .init();
}
