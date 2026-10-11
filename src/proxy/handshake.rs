use sha2::{Digest, Sha224};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Vless,
    Trojan,
    Shadowsocks,
}

/// 解析出的上游目标 + 握手残留数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub protocol: Protocol,
    pub host: String,
    pub port: u16,
    /// 建立 TCP 连接后需要立刻转发给上游的字节。
    pub tail: Vec<u8>,
    /// 需要先回复给客户端的字节（仅 VLESS）。
    pub ack: Option<Vec<u8>>,
}

fn read_u16be(data: &[u8], offset: usize) -> Option<u16> {
    let bytes: [u8; 2] = data.get(offset..offset + 2)?.try_into().ok()?;
    Some(u16::from_be_bytes(bytes))
}

/// ATYP 0x01/0x03/0x04 的通用地址解析，返回 (host, 下一个偏移)。
fn parse_address(data: &[u8], offset: usize) -> Option<(String, usize)> {
    let atyp = *data.get(offset)?;
    let offset = offset + 1;
    match atyp {
        0x01 => {
            let bytes: [u8; 4] = data.get(offset..offset + 4)?.try_into().ok()?;
            Some((bytes.map(|byte| byte.to_string()).join("."), offset + 4))
        }
        0x03 => {
            let len = *data.get(offset)? as usize;
            let offset = offset + 1;
            let host = String::from_utf8(data.get(offset..offset + len)?.to_vec()).ok()?;
            Some((host, offset + len))
        }
        0x04 => {
            let bytes: [u8; 16] = data.get(offset..offset + 16)?.try_into().ok()?;
            let host = bytes
                .chunks(2)
                .map(|pair| format!("{:x}", u16::from_be_bytes([pair[0], pair[1]])))
                .collect::<Vec<_>>()
                .join(":");
            Some((host, offset + 16))
        }
        _ => None,
    }
}

/// VLESS: version(1) | user id(16) | extra len(1) | [extra] | command(1) | port(2) | atyp | addr
fn parse_vless(msg: &[u8], uuid_hex: &str) -> Option<Target> {
    if msg.len() <= 17 || uuid_hex.len() < 32 {
        return None;
    }
    let id = &msg[1..17];
    let expected: Vec<u8> = (0..16)
        .map(|i| u8::from_str_radix(&uuid_hex[i * 2..i * 2 + 2], 16).ok())
        .collect::<Option<Vec<_>>>()?;
    if id != expected.as_slice() {
        return None;
    }

    let version = msg[0];
    let extra_len = msg[17] as usize;
    let mut offset = extra_len + 19;
    let port = read_u16be(msg, offset)?;
    offset += 2;
    let (host, offset) = parse_address(msg, offset)?;

    Some(Target {
        protocol: Protocol::Vless,
        host,
        port,
        tail: msg.get(offset..).unwrap_or(&[]).to_vec(),
        ack: Some(vec![version, 0x00]),
    })
}

fn sha224_hex(value: &str) -> String {
    let digest = Sha224::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Trojan: sha224(password) | CRLF | command | atyp | addr | port | CRLF
fn parse_trojan(msg: &[u8], uuid: &str) -> Option<Target> {
    if msg.len() < 58 {
        return None;
    }
    if &msg[0..56] != sha224_hex(uuid).as_bytes() {
        return None;
    }

    let mut offset = 56;
    if msg.get(offset..offset + 2) == Some([0x0d, 0x0a].as_slice()) {
        offset += 2;
    }
    if msg.get(offset) != Some(&0x01) {
        return None;
    }
    offset += 1;

    let (host, mut offset) = parse_address(msg, offset)?;
    let port = read_u16be(msg, offset)?;
    offset += 2;
    if msg.get(offset..offset + 2) == Some([0x0d, 0x0a].as_slice()) {
        offset += 2;
    }

    Some(Target {
        protocol: Protocol::Trojan,
        host,
        port,
        tail: msg.get(offset..).unwrap_or(&[]).to_vec(),
        ack: None,
    })
}

/// SS (v2ray-plugin websocket): atyp | addr | port
fn parse_shadowsocks(msg: &[u8]) -> Option<Target> {
    let (host, mut offset) = parse_address(msg, 0)?;
    let port = read_u16be(msg, offset)?;
    offset += 2;
    Some(Target {
        protocol: Protocol::Shadowsocks,
        host,
        port,
        tail: msg.get(offset..).unwrap_or(&[]).to_vec(),
        ack: None,
    })
}

/// 按 index.js 的顺序依次尝试 VLESS / Trojan / SS。
pub fn detect(msg: &[u8], uuid: &str, uuid_hex: &str) -> Option<Target> {
    if msg.len() > 17
        && msg[0] == 0x00
        && let Some(target) = parse_vless(msg, uuid_hex)
    {
        return Some(target);
    }
    if msg.len() >= 58
        && let Some(target) = parse_trojan(msg, uuid)
    {
        return Some(target);
    }
    if let Some(&first) = msg.first()
        && matches!(first, 0x01 | 0x03 | 0x04)
    {
        return parse_shadowsocks(msg);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: &str = "d1cf4b9c-3e57-085d-b34a-797fcf601381";
    const UUID_HEX: &str = "d1cf4b9c3e57085db34a797fcf601381";
    const PASSWORD_HASH: &str = "265322f36b5ab75a61f1f4cec80565f61e8e80a1a54f82de443c3eb3";

    fn uuid_bytes() -> Vec<u8> {
        (0..16)
            .map(|i| u8::from_str_radix(&UUID_HEX[i * 2..i * 2 + 2], 16).unwrap())
            .collect()
    }

    fn domain_bytes(host: &str) -> Vec<u8> {
        let mut out = vec![0x03, host.len() as u8];
        out.extend_from_slice(host.as_bytes());
        out
    }

    #[test]
    fn parses_vless_domain_request() {
        let mut msg = vec![0x00];
        msg.extend(uuid_bytes());
        msg.push(0x00); // 无附加字段
        msg.push(0x01); // command
        msg.extend([0x1f, 0x90]); // port 8080
        msg.extend(domain_bytes("example.com"));
        msg.extend_from_slice(b"tail-bytes");

        let target = detect(&msg, UUID, UUID_HEX).unwrap();
        assert_eq!(target.protocol, Protocol::Vless);
        assert_eq!(target.host, "example.com");
        assert_eq!(target.port, 8080);
        assert_eq!(target.ack, Some(vec![0x00, 0x00]));
        assert_eq!(target.tail, b"tail-bytes");
    }

    #[test]
    fn parses_vless_with_extra_fields_and_ipv4() {
        let mut msg = vec![0x00];
        msg.extend(uuid_bytes());
        msg.push(0x02); // 2 字节附加字段
        msg.extend([0xaa, 0xbb]);
        msg.push(0x01);
        msg.extend([0x01, 0xbb]); // port 443
        msg.extend([0x01, 93, 184, 216, 34]);
        let target = detect(&msg, UUID, UUID_HEX).unwrap();
        assert_eq!(target.host, "93.184.216.34");
        assert_eq!(target.port, 443);
        assert!(target.tail.is_empty());
    }

    #[test]
    fn rejects_vless_with_wrong_uuid() {
        let mut msg = vec![0x00];
        msg.extend(std::iter::repeat_n(0xab, 16));
        msg.extend([0x00, 0x01, 0x01, 0xbb, 0x01, 1, 2, 3, 4]);
        assert!(parse_vless(&msg, UUID_HEX).is_none());
    }

    #[test]
    fn parses_trojan_request() {
        let mut msg = PASSWORD_HASH.as_bytes().to_vec();
        msg.extend([0x0d, 0x0a, 0x01]);
        msg.extend(domain_bytes("www.baidu.com"));
        msg.extend([0x01, 0xbb]); // port 443
        msg.extend([0x0d, 0x0a]);
        msg.extend_from_slice(b"GET / HTTP/1.1\r\n");

        let target = detect(&msg, UUID, UUID_HEX).unwrap();
        assert_eq!(target.protocol, Protocol::Trojan);
        assert_eq!(target.host, "www.baidu.com");
        assert_eq!(target.port, 443);
        assert_eq!(target.ack, None);
        assert_eq!(target.tail, b"GET / HTTP/1.1\r\n");
    }

    #[test]
    fn trojan_requires_correct_hash_and_connect_command() {
        let mut msg = PASSWORD_HASH.as_bytes().to_vec();
        msg.extend([0x0d, 0x0a, 0x02]); // command != 0x01
        msg.extend(domain_bytes("example.com"));
        msg.extend([0x01, 0xbb]);
        assert!(parse_trojan(&msg, UUID).is_none());

        let mut msg = vec![0x30u8; 56];
        msg.extend([0x0d, 0x0a, 0x01]);
        msg.extend(domain_bytes("example.com"));
        msg.extend([0x01, 0xbb]);
        assert!(parse_trojan(&msg, UUID).is_none());
    }

    #[test]
    fn parses_shadowsocks_request() {
        let mut msg = domain_bytes("example.org");
        msg.extend([0x00, 0x50]); // port 80
        msg.extend_from_slice(b"payload");

        let target = detect(&msg, UUID, UUID_HEX).unwrap();
        assert_eq!(target.protocol, Protocol::Shadowsocks);
        assert_eq!(target.host, "example.org");
        assert_eq!(target.port, 80);
        assert_eq!(target.tail, b"payload");
    }

    #[test]
    fn parses_ipv6_target() {
        let mut msg = vec![0x04];
        msg.extend([
            0x20, 0x01, 0x0d, 0xb8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x01,
        ]);
        msg.extend([0x01, 0xbb]);
        let target = parse_shadowsocks(&msg).unwrap();
        assert_eq!(target.host, "2001:db8:0:0:0:0:0:1");
        assert_eq!(target.port, 443);
    }

    #[test]
    fn rejects_unknown_first_byte() {
        assert!(detect(&[0x17, 0x03, 0x01, 0x00], UUID, UUID_HEX).is_none());
        assert!(detect(&[], UUID, UUID_HEX).is_none());
    }

    #[test]
    fn prefers_vless_over_ss_when_both_parse() {
        // 首字节 0x00 且 uuid 匹配时按 VLESS 处理
        let mut msg = vec![0x00];
        msg.extend(uuid_bytes());
        msg.extend([0x00, 0x01, 0x01, 0xbb, 0x01, 1, 2, 3, 4]);
        assert_eq!(
            detect(&msg, UUID, UUID_HEX).unwrap().protocol,
            Protocol::Vless
        );
    }
}
