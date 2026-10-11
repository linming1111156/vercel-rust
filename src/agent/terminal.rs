use std::time::Duration;

use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};

use crate::agent::{IoStream, pty, stream_id_frame};
use crate::proto::IoStreamData;

const KEEPALIVE: Duration = Duration::from_secs(30);
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 40;

#[derive(Debug, Deserialize)]
struct TerminalTask {
    #[serde(default, rename = "StreamID", alias = "streamId")]
    stream_id: String,
}

#[derive(Debug, Deserialize)]
struct Resize {
    #[serde(default, rename = "Cols", alias = "cols")]
    cols: Option<u16>,
    #[serde(default, rename = "Rows", alias = "rows")]
    rows: Option<u16>,
}

/// 终端任务（type 8）：IOStream <-> PTY 双向转发。
pub async fn run(data: String, stream: IoStream) {
    let (sender, mut inbound) = stream;

    let Ok(task) = serde_json::from_str::<TerminalTask>(&data) else {
        tracing::error!("[Terminal] 任务解析错误: {data}");
        return;
    };
    tracing::debug!("[Terminal] 初始化, StreamID: {}", task.stream_id);

    let handshake = IoStreamData {
        data: stream_id_frame(&task.stream_id),
    };
    if sender.send(handshake).await.is_err() {
        tracing::error!("[Terminal] 发送 StreamID 失败");
        return;
    }

    let Some(terminal) = pty::spawn() else {
        return;
    };
    let pty::Terminal {
        input,
        mut output,
        mut exit,
        backend,
    } = terminal;

    let mut keepalive = interval(KEEPALIVE);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Skip);
    keepalive.tick().await;

    loop {
        tokio::select! {
            frame = inbound.message() => match frame {
                Ok(Some(message)) => {
                    if !handle_frame(&message.data, &input, &backend).await {
                        break;
                    }
                }
                Ok(None) => break,
                Err(status) => {
                    tracing::error!("[Terminal] IOStream 错误: {status}");
                    break;
                }
            },
            output_data = output.recv() => match output_data {
                Some(bytes) => {
                    let payload = IoStreamData { data: bytes };
                    if sender.send(payload).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            status = &mut exit => {
                tracing::debug!("[Terminal] 退出, StreamID: {}, code: {:?}", task.stream_id, status);
                break;
            },
            _ = keepalive.tick() => {
                let payload = IoStreamData { data: Vec::new() };
                if sender.send(payload).await.is_err() {
                    break;
                }
            }
        }
    }

    drop(input);
    backend.kill();
}

/// 控制帧：0 = 输入数据，1 = 调整窗口大小。返回 false 表示会话应结束。
async fn handle_frame(data: &[u8], input: &mpsc::Sender<Vec<u8>>, backend: &pty::Backend) -> bool {
    let Some(&command) = data.first() else {
        return true;
    };
    match command {
        0 => input.send(data[1..].to_vec()).await.is_ok(),
        1 => {
            if let Ok(resize) = serde_json::from_slice::<Resize>(&data[1..]) {
                backend.resize(
                    resize.cols.unwrap_or(DEFAULT_COLS),
                    resize.rows.unwrap_or(DEFAULT_ROWS),
                );
            }
            true
        }
        other => {
            tracing::debug!("[Terminal] 未知控制帧: {other}");
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_task_and_resize() {
        let task: TerminalTask = serde_json::from_str(r#"{"TaskID":3,"StreamID":"42"}"#).unwrap();
        assert_eq!(task.stream_id, "42");

        let resize: Resize = serde_json::from_str(r#"{"Cols":120,"Rows":30}"#).unwrap();
        assert_eq!(resize.cols, Some(120));
        assert_eq!(resize.rows, Some(30));

        let empty: Resize = serde_json::from_str("{}").unwrap();
        assert_eq!(empty.cols, None);
    }
}
