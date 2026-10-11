use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::extract::ws::{Message, WebSocket};
use futures_util::ready;
use futures_util::sink::Sink;
use futures_util::stream::{SplitSink, SplitStream, Stream, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// 把 axum 的 WebSocket 适配成 AsyncRead + AsyncWrite，
/// 对应 index.js 里的 createWebSocketStream(ws)。
pub struct WsDuplex {
    sink: SplitSink<WebSocket, Message>,
    stream: SplitStream<WebSocket>,
    pending: VecDeque<u8>,
    eof: bool,
}

impl WsDuplex {
    pub fn new(socket: WebSocket) -> Self {
        let (sink, stream) = socket.split();
        Self {
            sink,
            stream,
            pending: VecDeque::new(),
            eof: false,
        }
    }

    /// 转发结束后关闭 WebSocket 连接。
    pub async fn shutdown(mut self) {
        let _ = futures_util::sink::SinkExt::close(&mut self.sink).await;
    }
}

fn map_io(result: Poll<Result<(), axum::Error>>) -> Poll<io::Result<()>> {
    match result {
        Poll::Ready(Ok(())) => Poll::Ready(Ok(())),
        Poll::Ready(Err(error)) => Poll::Ready(Err(io::Error::other(error))),
        Poll::Pending => Poll::Pending,
    }
}

impl AsyncRead for WsDuplex {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if !self.pending.is_empty() {
                let len = buf.remaining().min(self.pending.len());
                for _ in 0..len {
                    let byte = self.pending.pop_front().expect("non-empty");
                    buf.put_slice(&[byte]);
                }
                return Poll::Ready(Ok(()));
            }
            if self.eof {
                return Poll::Ready(Ok(()));
            }

            match ready!(Pin::new(&mut self.stream).poll_next(cx)) {
                Some(Ok(Message::Binary(data))) => {
                    self.pending.extend(data);
                }
                Some(Ok(Message::Text(text))) => {
                    self.pending.extend(text.as_str().as_bytes());
                }
                // axum 会自动回复 ping，这里只需要忽略控制帧。
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                    self.eof = true;
                }
            }
        }
    }
}

impl AsyncWrite for WsDuplex {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.eof {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "websocket closed",
            )));
        }
        match Pin::new(&mut self.sink).poll_ready(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(error)) => return Poll::Ready(Err(io::Error::other(error))),
            Poll::Pending => return Poll::Pending,
        }
        match Pin::new(&mut self.sink).start_send(Message::Binary(buf.to_vec().into())) {
            Ok(()) => Poll::Ready(Ok(buf.len())),
            Err(error) => Poll::Ready(Err(io::Error::other(error))),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        map_io(Pin::new(&mut self.sink).poll_flush(cx))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        map_io(Pin::new(&mut self.sink).poll_close(cx))
    }
}
