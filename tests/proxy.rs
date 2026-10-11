use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::prelude::BASE64_STANDARD as STANDARD;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use nzws_rust::app::AppState;
use nzws_rust::config::Config;
use nzws_rust::web::router;

const UUID: &str = "d1cf4b9c-3e57-085d-b34a-797fcf601381";

async fn spawn_server(config: Config) -> SocketAddr {
    let state = AppState::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router(state)).await;
    });
    addr
}

fn test_config() -> Config {
    let mut config = Config::from_env();
    config.uuid = UUID.to_string();
    config.ws_path = "testpath".to_string();
    config.sub_path = "testsub".to_string();
    config.name = "TestNode".to_string();
    config.domain = "example.com".to_string();
    config.nezha_server = String::new();
    config.nezha_key = String::new();
    config
}

/// 一个 TCP echo 服务，作为 WebSocket 代理的上游。
async fn spawn_echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 4096];
                loop {
                    match socket.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(size) => {
                            if socket.write_all(&buffer[..size]).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
    });
    port
}

async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status: u16 = response
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or_default();
    (status, response)
}

#[tokio::test]
async fn serves_index_page_and_404() {
    let addr = spawn_server(test_config()).await;

    let (status, body) = get(addr, "/").await;
    assert_eq!(status, 200);
    assert!(body.contains("text/html"));

    let (status, body) = get(addr, "/definitely-not-a-route").await;
    assert_eq!(status, 404);
    assert!(body.contains("Not Found"));
}

#[tokio::test]
async fn serves_base64_subscription() {
    let addr = spawn_server(test_config()).await;
    let (status, response) = get(addr, "/testsub").await;
    assert_eq!(status, 200);

    let body = response.split_once("\r\n\r\n").expect("http body").1;
    let decoded = String::from_utf8(STANDARD.decode(body.trim()).unwrap()).unwrap();
    let lines: Vec<&str> = decoded.lines().collect();
    assert_eq!(lines.len(), 3, "vless + trojan + ss");
    assert!(lines[0].starts_with(&format!("vless://{UUID}@example.com:443")));
    assert!(lines[1].starts_with(&format!("trojan://{UUID}@example.com:443")));
    assert!(lines[2].contains("plugin=v2ray-plugin"));
    assert!(decoded.contains("#TestNode-"));
}

#[tokio::test]
async fn proxies_shadowsocks_over_websocket() {
    let upstream = spawn_echo_server().await;
    let addr = spawn_server(test_config()).await;

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/testpath"))
        .await
        .unwrap();

    // v2ray-plugin 首包：atyp(0x01) + IPv4 + 端口
    let mut handshake = vec![0x01];
    handshake.extend_from_slice(&[127, 0, 0, 1]);
    handshake.extend_from_slice(&upstream.to_be_bytes());
    socket
        .send(Message::Binary(handshake.into()))
        .await
        .unwrap();

    socket
        .send(Message::Binary(b"hello proxy".to_vec().into()))
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("timed out waiting for echo")
        .expect("websocket message")
        .expect("no error");

    assert_eq!(&reply.into_data()[..], b"hello proxy");
}

#[tokio::test]
async fn closes_connections_on_unknown_path() {
    let addr = spawn_server(test_config()).await;
    // 路径不匹配时服务端会立即关闭连接（与 index.js 的 wss 行为一致）
    let Ok((mut socket, _)) =
        tokio_tungstenite::connect_async(format!("ws://{addr}/wrong-path")).await
    else {
        return;
    };
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("no hang");
    assert!(matches!(
        message,
        None | Some(Ok(Message::Close(_))) | Some(Err(_))
    ));
}

#[tokio::test]
async fn closes_websocket_upgrades_on_page_routes() {
    let addr = spawn_server(test_config()).await;

    // 伪装页 / 订阅路径上的 WebSocket 升级不应该建立代理连接
    for path in ["/", "/testsub"] {
        let Ok((mut socket, _)) =
            tokio_tungstenite::connect_async(format!("ws://{addr}{path}")).await
        else {
            continue;
        };
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("no hang");
        assert!(
            matches!(message, None | Some(Ok(Message::Close(_))) | Some(Err(_))),
            "unexpected message on {path}"
        );
    }
}

#[test]
fn shared_state_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Arc<nzws_rust::app::AppState>>();
}

fn vless_handshake(port: u16) -> Vec<u8> {
    let uuid_hex = UUID.replace('-', "");
    let mut msg = vec![0x00];
    for i in 0..16 {
        msg.push(u8::from_str_radix(&uuid_hex[i * 2..i * 2 + 2], 16).unwrap());
    }
    msg.push(0x00); // 无附加字段
    msg.push(0x01); // command
    msg.extend_from_slice(&port.to_be_bytes());
    msg.extend([0x01, 127, 0, 0, 1]); // ATYP IPv4
    msg
}

fn trojan_handshake(port: u16, payload: &[u8]) -> Vec<u8> {
    // 密码 = sha224(UUID) 的 56 位十六进制字符串，与 index.js 一致
    let hash = "265322f36b5ab75a61f1f4cec80565f61e8e80a1a54f82de443c3eb3";
    let mut msg = hash.as_bytes().to_vec();
    msg.extend([0x0d, 0x0a, 0x01]);
    msg.extend([0x01, 127, 0, 0, 1]);
    msg.extend_from_slice(&port.to_be_bytes());
    msg.extend([0x0d, 0x0a]);
    msg.extend_from_slice(payload);
    msg
}

#[tokio::test]
async fn proxies_vless_over_websocket() {
    let upstream = spawn_echo_server().await;
    let addr = spawn_server(test_config()).await;

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/testpath"))
        .await
        .unwrap();
    socket
        .send(Message::Binary(vless_handshake(upstream).into()))
        .await
        .unwrap();

    // VLESS 服务端回执：[version, 0]
    let ack = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("timed out")
        .expect("no io error")
        .expect("message");
    assert_eq!(&ack.into_data()[..], &[0x00, 0x00]);

    socket
        .send(Message::Binary(b"vless payload".to_vec().into()))
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("timed out")
        .expect("no io error")
        .expect("message");
    assert_eq!(&reply.into_data()[..], b"vless payload");
}

#[tokio::test]
async fn proxies_trojan_over_websocket() {
    let upstream = spawn_echo_server().await;
    let addr = spawn_server(test_config()).await;

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/testpath"))
        .await
        .unwrap();
    socket
        .send(Message::Binary(
            trojan_handshake(upstream, b"GET / HTTP/1.1\r\n").into(),
        ))
        .await
        .unwrap();

    // 握手残留字节会被转发的同时，后续帧也应该继续透传
    let reply = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("timed out")
        .expect("no io error")
        .expect("message");
    assert_eq!(&reply.into_data()[..], b"GET / HTTP/1.1\r\n");

    socket
        .send(Message::Binary(b"trojan body".to_vec().into()))
        .await
        .unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("timed out")
        .expect("no io error")
        .expect("message");
    assert_eq!(&reply.into_data()[..], b"trojan body");
}

#[tokio::test]
async fn rejects_wrong_credentials() {
    let upstream = spawn_echo_server().await;
    let addr = spawn_server(test_config()).await;

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/testpath"))
        .await
        .unwrap();

    // uuid 不匹配的 VLESS 首包，且首字节不是合法的 SS atyp -> 直接关闭
    let mut msg = vless_handshake(upstream);
    msg[1] = 0xee;
    msg[0] = 0x09;
    socket.send(Message::Binary(msg.into())).await.unwrap();

    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("no hang");
    assert!(matches!(
        message,
        None | Some(Err(_)) | Some(Ok(Message::Close(_)))
    ));
}
