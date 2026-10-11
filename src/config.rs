use std::env;

pub const DEFAULT_UUID: &str = "d1cf4b9c-3e57-085d-b34a-797fcf601381";
pub const PLACEHOLDER_DOMAIN: &str = "your-domain.com";
pub const AGENT_VERSION: &str = "rust-9.9.9";

pub const REPORT_DELAY_SECS: u64 = 4;
pub const RETRY_DELAY_SECS: u64 = 10_000;
pub const IP_REPORT_PERIOD_SECS: u64 = 1800;
pub const NETWORK_TIMEOUT_SECS: u64 = 8;
pub const HOST_REPORT_PERIOD_SECS: u64 = 600;

pub const TLS_PORTS: [u16; 6] = [443, 2053, 2083, 2087, 2096, 8443];

pub const DNS_SERVERS: [&str; 2] = ["8.8.4.4", "1.1.1.1"];

pub const BLOCKED_DOMAINS: [&str; 10] = [
    "speedtest.net",
    "fast.com",
    "speedtest.cn",
    "speed.cloudflare.com",
    "speedof.me",
    "testmy.net",
    "bandwidth.place",
    "speed.io",
    "librespeed.org",
    "speedcheck.org",
];

#[derive(Debug, Clone)]
pub struct Config {
    pub uuid: String,
    pub nezha_server: String,
    pub nezha_key: String,
    pub domain: String,
    pub auto_access: bool,
    pub sub_path: String,
    pub name: String,
    pub port: u16,
    pub show_log: bool,
    pub ws_path: String,
}

fn env_or(key: &str, default: &str) -> String {
    match env::var(key) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => default.to_string(),
    }
}

fn env_port(key: &str, default: u16) -> u16 {
    env::var(key)
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_flag(key: &str) -> bool {
    match env::var(key) {
        Ok(value) => {
            let value = value.trim().to_lowercase();
            !value.is_empty() && value != "false" && value != "0" && value != "off"
        }
        Err(_) => false,
    }
}

impl Config {
    pub fn from_env() -> Self {
        let uuid = env_or("UUID", DEFAULT_UUID);
        let ws_path = env_or("WSPATH", &uuid.chars().take(8).collect::<String>());
        let sub_path = env_or("SUB_PATH", "vercel").trim_matches('/').to_string();
        Self {
            nezha_server: env_or("NEZHA_SERVER", ""),
            nezha_key: env_or("NEZHA_KEY", ""),
            domain: env_or("DOMAIN", PLACEHOLDER_DOMAIN),
            auto_access: env_flag("AUTO_ACCESS"),
            name: env_or("NAME", "Vercel"),
            port: env_port("PORT", 3000),
            show_log: env_flag("SHOW_LOG"),
            ws_path: ws_path.trim_matches('/').to_string(),
            uuid,
            sub_path,
        }
    }

    /// UUID without dashes, used as the VLESS user id.
    pub fn uuid_hex(&self) -> String {
        self.uuid.replace('-', "")
    }

    pub fn is_placeholder_domain(&self) -> bool {
        self.domain.is_empty() || self.domain == PLACEHOLDER_DOMAIN
    }
}

/// NZ-TLS: 端口在 TLS 端口列表内时使用 TLS。
pub fn should_use_tls(server: &str) -> bool {
    server
        .rsplit(':')
        .next()
        .and_then(|port| port.parse::<u16>().ok())
        .is_some_and(|port| TLS_PORTS.contains(&port))
}

pub fn is_blocked_domain(host: &str) -> bool {
    if host.is_empty() {
        return false;
    }
    let host = host.to_lowercase();
    BLOCKED_DOMAINS
        .iter()
        .any(|blocked| host == *blocked || host.ends_with(&format!(".{blocked}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_tls_by_port() {
        assert!(should_use_tls("example.com:443"));
        assert!(should_use_tls("example.com:8443"));
        assert!(!should_use_tls("example.com:8080"));
        assert!(!should_use_tls("example.com"));
    }

    #[test]
    fn blocks_speedtest_domains() {
        assert!(is_blocked_domain("speed.cloudflare.com"));
        assert!(is_blocked_domain("SPEEDTEST.NET"));
        assert!(is_blocked_domain("a.fast.com"));
        assert!(!is_blocked_domain("fast.com.example.com"));
        assert!(!is_blocked_domain(""));
    }

    #[test]
    fn strips_dashes_from_uuid() {
        let mut config = Config::from_env();
        config.uuid = "d1cf4b9c-3e57-085d-b34a-797fcf601381".to_string();
        assert_eq!(config.uuid_hex(), "d1cf4b9c3e57085db34a797fcf601381");
    }
}
