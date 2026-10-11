use std::time::Duration;

use base64::Engine;
use base64::prelude::BASE64_STANDARD as STANDARD;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::config::Config;

const ISP_TIMEOUT: Duration = Duration::from_secs(3);
const IP_TIMEOUT: Duration = Duration::from_secs(5);

/// 订阅链接实际指向的接入点。
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub domain: String,
    pub port: u16,
    pub tls: bool,
}

#[derive(Deserialize)]
struct IpSbGeoIp {
    #[serde(rename = "country_code")]
    country_code: Option<String>,
    #[serde(rename = "isp")]
    isp: Option<String>,
}

#[derive(Deserialize)]
struct IpApiGeoIp {
    #[serde(rename = "countryCode")]
    country_code: Option<String>,
    #[serde(rename = "org")]
    org: Option<String>,
}

async fn get_json<T: DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
) -> Option<T> {
    let response = client
        .get(url)
        .timeout(timeout)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    response.json::<T>().await.ok()
}

fn sanitize(value: &str) -> String {
    value.replace(' ', "_")
}

/// 获取 isp（对应 index.js 的 getisp）。
pub async fn fetch_isp(client: &reqwest::Client) -> String {
    if let Some(data) = get_json::<IpSbGeoIp>(client, "https://api.ip.sb/geoip", ISP_TIMEOUT).await
    {
        return sanitize(&format!(
            "{}-{}",
            data.country_code.unwrap_or_default(),
            data.isp.unwrap_or_default()
        ));
    }

    if let Some(data) = get_json::<IpApiGeoIp>(client, "http://ip-api.com/json", ISP_TIMEOUT).await
    {
        return sanitize(&format!(
            "{}-{}",
            data.country_code.unwrap_or_default(),
            data.org.unwrap_or_default()
        ));
    }

    "Unknown".to_string()
}

/// 获取接入域名/IP（对应 index.js 的 getip）。
pub async fn resolve_endpoint(config: &Config, client: &reqwest::Client) -> Endpoint {
    if !config.is_placeholder_domain() {
        return Endpoint {
            domain: config.domain.clone(),
            port: 443,
            tls: true,
        };
    }

    let body = match client
        .get("https://api-ipv4.ip.sb/ip")
        .timeout(IP_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => match response.error_for_status() {
            Ok(response) => response.text().await.ok(),
            Err(_) => None,
        },
        Err(_) => None,
    };

    match body {
        Some(body) if !body.trim().is_empty() => Endpoint {
            domain: body.trim().to_string(),
            port: config.port,
            tls: false,
        },
        _ => {
            tracing::error!("Failed to get IP");
            Endpoint {
                domain: "cahnge-your-domain.com".to_string(),
                port: 443,
                tls: true,
            }
        }
    }
}

/// 生成 base64 订阅内容（VLESS + Trojan + SS/v2ray-plugin）。
pub fn build_subscription(config: &Config, endpoint: &Endpoint, isp: &str) -> String {
    let name = if config.name.is_empty() {
        isp.to_string()
    } else {
        format!("{}-{}", config.name, isp)
    };

    let security = if endpoint.tls { "tls" } else { "none" };
    let ss_tls = if endpoint.tls { "tls;" } else { "" };
    let domain = &endpoint.domain;
    let port = endpoint.port;
    let path = format!("%2F{}", config.ws_path);
    let uuid = &config.uuid;

    let vless = format!(
        "vless://{uuid}@{domain}:{port}?encryption=none&security={security}&sni={domain}&fp=chrome&type=ws&host={domain}&path={path}#{name}"
    );
    let trojan = format!(
        "trojan://{uuid}@{domain}:{port}?security={security}&sni={domain}&fp=chrome&type=ws&host={domain}&path={path}#{name}"
    );
    let method_password = STANDARD.encode(format!("none:{uuid}"));
    let shadowsocks = format!(
        "ss://{method_password}@{domain}:{port}?plugin=v2ray-plugin;mode%3Dwebsocket;host%3D{domain};path%3D{path};{ss_tls}sni%3D{domain};skip-cert-verify%3Dtrue;mux%3D0#{name}"
    );

    let subscription = format!("{vless}\n{trojan}\n{shadowsocks}");
    format!("{}\n", STANDARD.encode(subscription))
}

/// 一次订阅请求：先取 isp / 接入点，再拼链接。
pub async fn subscription_body(config: &Config, client: &reqwest::Client) -> String {
    let (isp, endpoint) = tokio::join!(fetch_isp(client), resolve_endpoint(config, client));
    build_subscription(config, &endpoint, &isp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        let mut config = Config::from_env();
        config.uuid = "d1cf4b9c-3e57-085d-b34a-797fcf601381".to_string();
        config.ws_path = "d1cf4b9c".to_string();
        config.name = "Vercel".to_string();
        config
    }

    #[test]
    fn builds_tls_links() {
        let config = config();
        let endpoint = Endpoint {
            domain: "example.com".to_string(),
            port: 443,
            tls: true,
        };
        let body = build_subscription(&config, &endpoint, "AS-Cloudflare");
        let decoded = String::from_utf8(STANDARD.decode(body.trim()).unwrap()).unwrap();
        let lines: Vec<&str> = decoded.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            "vless://d1cf4b9c-3e57-085d-b34a-797fcf601381@example.com:443?encryption=none&security=tls&sni=example.com&fp=chrome&type=ws&host=example.com&path=%2Fd1cf4b9c#Vercel-AS-Cloudflare"
        );
        assert!(lines[1].starts_with("trojan://"));
        assert!(lines[1].contains("security=tls"));
        assert!(lines[2].contains("plugin=v2ray-plugin"));
        assert!(lines[2].contains("tls;sni%3Dexample.com"));
        assert_eq!(
            STANDARD
                .decode("bm9uZTpkMWNmNGI5Yy0zZTU3LTA4NWQtYjM0YS03OTdmY2Y2MDEzODE=")
                .unwrap(),
            b"none:d1cf4b9c-3e57-085d-b34a-797fcf601381"
        );
    }

    #[test]
    fn builds_plain_links_without_tls() {
        let config = config();
        let endpoint = Endpoint {
            domain: "1.2.3.4".to_string(),
            port: 3000,
            tls: false,
        };
        let body = build_subscription(&config, &endpoint, "Unknown");
        let decoded = String::from_utf8(STANDARD.decode(body.trim()).unwrap()).unwrap();
        assert!(decoded.contains("security=none"));
        assert!(!decoded.contains("none;sni%3D"));
        assert!(decoded.contains("1.2.3.4:3000"));
    }
}
