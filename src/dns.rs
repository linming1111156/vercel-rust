use std::net::IpAddr;

use serde::Deserialize;

use crate::config::DNS_SERVERS;

#[derive(Debug, Deserialize)]
struct DnsAnswer {
    #[serde(rename = "type")]
    record_type: u32,
    data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DnsResponse {
    status: Option<u32>,
    answer: Option<Vec<DnsAnswer>>,
}

/// Custom DNS：通过 DoH 解析域名，失败时返回 None（调用方回退到系统解析）。
pub async fn resolve_host(client: &reqwest::Client, host: &str) -> Option<String> {
    if host.parse::<IpAddr>().is_ok() {
        return Some(host.to_string());
    }

    for server in DNS_SERVERS {
        let request = client
            .get("https://dns.google/resolve")
            .query(&[("name", host), ("type", "A"), ("server", server)])
            .header("Accept", "application/dns-json");
        let Ok(response) = request.send().await else {
            continue;
        };
        let Ok(parsed) = response.json::<DnsResponse>().await else {
            continue;
        };
        if parsed.status != Some(0) {
            continue;
        }
        if let Some(record) = parsed
            .answer
            .iter()
            .flatten()
            .find(|record| record.record_type == 1)
            .and_then(|record| record.data.as_ref())
        {
            return Some(record.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn passes_through_ip_literals() {
        let client = crate::net::client(std::time::Duration::from_secs(5));
        assert_eq!(
            resolve_host(&client, "93.184.216.34").await.as_deref(),
            Some("93.184.216.34")
        );
        assert_eq!(resolve_host(&client, "::1").await.as_deref(), Some("::1"));
    }
}
