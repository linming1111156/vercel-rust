use std::net::IpAddr;
use std::time::Duration;

const IPV4_ENDPOINTS: [&str; 3] = [
    "https://ipv4.ip.sb/ip",
    "https://blog.cloudflare.com/cdn-cgi/trace",
    "https://developers.cloudflare.com/cdn-cgi/trace",
];
const IPV6_ENDPOINTS: [&str; 3] = [
    "https://ipv6.ip.sb/ip",
    "https://blog.cloudflare.com/cdn-cgi/trace",
    "https://developers.cloudflare.com/cdn-cgi/trace",
];

const IP_TIMEOUT: Duration = Duration::from_secs(10);

/// 依次尝试多个探测点，返回第一个匹配地址族的 IP。
async fn fetch_from_endpoints(client: &reqwest::Client, endpoints: &[&str], v6: bool) -> String {
    for url in endpoints.iter().copied() {
        let Ok(response) = client.get(url).send().await else {
            continue;
        };
        let Ok(body) = response.text().await else {
            continue;
        };
        let ip = parse_ip_from_response(&body, v6);
        if !ip.is_empty() {
            return ip;
        }
    }
    String::new()
}

/// 同时获取公网 IPv4 / IPv6（对应 index.js 的 fetchIP）。
pub async fn fetch_public_ips() -> (String, String) {
    let v4 = crate::net::client_v4(IP_TIMEOUT);
    let v6 = crate::net::client_v6(IP_TIMEOUT);
    let (ipv4, ipv6) = tokio::join!(
        fetch_from_endpoints(&v4, &IPV4_ENDPOINTS, false),
        fetch_from_endpoints(&v6, &IPV6_ENDPOINTS, true)
    );
    (ipv4, ipv6)
}

/// 响应体可能是一行裸 IP，也可能是 cdn-cgi/trace 里的 `ip=x.x.x.x`。
fn parse_ip_from_response(body: &str, v6: bool) -> String {
    let trimmed = body.trim();
    if matches_family(trimmed, v6) {
        return trimmed.to_string();
    }
    for line in trimmed.lines() {
        if let Some(rest) = line.strip_prefix("ip=") {
            let ip = rest.trim();
            if matches_family(ip, v6) {
                return ip.to_string();
            }
        }
    }
    String::new()
}

fn matches_family(value: &str, v6: bool) -> bool {
    match value.parse::<IpAddr>() {
        Ok(IpAddr::V4(_)) => !v6,
        Ok(IpAddr::V6(_)) => v6,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_ip() {
        assert_eq!(parse_ip_from_response(" 1.2.3.4\n", false), "1.2.3.4");
        assert_eq!(parse_ip_from_response("2001:db8::1", true), "2001:db8::1");
        assert_eq!(parse_ip_from_response("2001:db8::1", false), "");
    }

    #[test]
    fn parses_trace_body() {
        let body = "fl=1a2b\nh=blog.cloudflare.com\nip=8.8.8.8\nproto=http/1.1\n";
        assert_eq!(parse_ip_from_response(body, false), "8.8.8.8");
    }

    #[test]
    fn ignores_garbage() {
        assert_eq!(parse_ip_from_response("ip=not-an-ip", false), "");
        assert_eq!(parse_ip_from_response("<html>error</html>", false), "");
    }
}
