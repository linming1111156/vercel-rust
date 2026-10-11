use std::net::IpAddr;
use std::time::Duration;

/// 通用 HTTP 客户端（带 UA 与整体超时）。
pub fn client(timeout: Duration) -> reqwest::Client {
    build_client(timeout, None)
}

/// 只走 IPv6 出口的 HTTP 客户端。
pub fn client_v6(timeout: Duration) -> reqwest::Client {
    build_client(timeout, Some(IpAddr::from([0, 0, 0, 0, 0, 0, 0, 0])))
}

/// 只走 IPv4 出口的 HTTP 客户端。
pub fn client_v4(timeout: Duration) -> reqwest::Client {
    build_client(timeout, Some(IpAddr::from([0, 0, 0, 0])))
}

fn build_client(timeout: Duration, local_address: Option<IpAddr>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .user_agent("Mozilla/5.0")
        .timeout(timeout)
        .connect_timeout(timeout.min(Duration::from_secs(5)));
    if let Some(address) = local_address {
        builder = builder.local_address(address);
    }
    builder.build().unwrap_or_default()
}
