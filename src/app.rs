use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;

/// 共享的应用状态：配置 + 复用的 HTTP 客户端。
pub struct AppState {
    pub config: Config,
    pub http: reqwest::Client,
}

impl AppState {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            http: crate::net::client(Duration::from_secs(10)),
        })
    }
}
