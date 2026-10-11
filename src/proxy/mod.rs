pub mod bridge;
pub mod handshake;

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use futures_util::sink::SinkExt;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::app::AppState;
use crate::config::is_blocked_domain;
use crate::dns::resolve_host;
use crate::proxy::handshake::detect;
use bridge::WsDuplex;

/// 单个 WebSocket 连接：解析首包 -> DoH 解析 -> TCP 桥接。
pub async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    let config = &state.config;
    let first = match next_binary(&mut socket).await {
        Some(message) => message,
        None => return,
    };

    let target = match detect(&first, &config.uuid, &config.uuid_hex()) {
        Some(target) => target,
        None => {
            tracing::debug!("[Proxy] 未知协议首包, 长度 {}", first.len());
            let _ = socket.close().await;
            return;
        }
    };

    if is_blocked_domain(&target.host) {
        tracing::debug!("[Proxy] 命中测速域名: {}", target.host);
        let _ = socket.close().await;
        return;
    }

    if let Some(ack) = &target.ack {
        let _ = socket.send(Message::Binary(ack.clone().into())).await;
    }

    let address = resolve_host(&state.http, &target.host)
        .await
        .unwrap_or_else(|| target.host.clone());

    let mut tcp = match TcpStream::connect((address.as_str(), target.port)).await {
        Ok(tcp) => tcp,
        Err(error) => {
            tracing::debug!(
                "[Proxy] 连接上游 {}:{} 失败: {error}",
                target.host,
                target.port
            );
            let _ = socket.close().await;
            return;
        }
    };

    if !target.tail.is_empty() && tcp.write_all(&target.tail).await.is_err() {
        let _ = socket.close().await;
        return;
    }

    tracing::debug!(
        "[Proxy] {:?} {}:{} -> {}:{}",
        target.protocol,
        target.host,
        target.port,
        address,
        target.port
    );

    let mut duplex = WsDuplex::new(socket);
    let _ = tokio::io::copy_bidirectional(&mut duplex, &mut tcp).await;
    duplex.shutdown().await;
}

/// 读取握手首包（二进制帧）。
async fn next_binary(socket: &mut WebSocket) -> Option<Vec<u8>> {
    loop {
        let message = socket.recv().await?.ok()?;
        match message {
            Message::Binary(data) => return Some(data.to_vec()),
            Message::Text(text) => return Some(text.as_str().as_bytes().to_vec()),
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(_) => return None,
        }
    }
}
