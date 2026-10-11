use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};

use crate::agent::{IoStream, stream_id_frame};
use crate::proto::IoStreamData;

const NZFN: &[u8; 4] = b"NZFN";
const NZTD: &[u8; 4] = b"NZTD";
const NERR: &[u8; 4] = b"NERR";
const NZUP: &[u8; 4] = b"NZUP";

const KEEPALIVE: Duration = Duration::from_secs(30);
const CHUNK_SIZE: usize = 1024 * 1024;

type Sender = mpsc::Sender<IoStreamData>;

#[derive(Debug, Deserialize)]
struct FmTask {
    #[serde(default, rename = "StreamID", alias = "streamId")]
    stream_id: String,
}

struct Upload {
    file: fs::File,
    size: u64,
    received: u64,
}

/// 文件管理任务（type 11）：列目录 / 下载 / 上传。
pub async fn run(data: String, stream: IoStream) {
    let (mut sender, mut inbound) = stream;

    let Ok(task) = serde_json::from_str::<FmTask>(&data) else {
        tracing::error!("[FM] 任务解析错误: {data}");
        return;
    };

    let handshake = IoStreamData {
        data: stream_id_frame(&task.stream_id),
    };
    if sender.send(handshake).await.is_err() {
        tracing::error!("[FM] 发送 StreamID 失败");
        return;
    }
    tracing::debug!("[FM] 初始化, StreamID: {}", task.stream_id);

    let mut keepalive = interval(KEEPALIVE);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Skip);
    keepalive.tick().await;

    let mut upload: Option<Upload> = None;
    loop {
        tokio::select! {
            frame = inbound.message() => match frame {
                Ok(Some(message)) => {
                    if message.data.is_empty() {
                        continue;
                    }
                    if !handle_frame(message.data, &mut upload, &mut sender).await {
                        break;
                    }
                }
                Ok(None) => {
                    tracing::debug!("[FM] IOStream 结束, StreamID: {}", task.stream_id);
                    break;
                }
                Err(status) => {
                    tracing::error!("[FM] IOStream 错误: {status}");
                    break;
                }
            },
            _ = keepalive.tick() => {
                let payload = IoStreamData { data: Vec::new() };
                if sender.send(payload).await.is_err() {
                    break;
                }
            }
        }
    }

    if let Some(mut upload) = upload.take() {
        let _ = upload.file.shutdown().await;
    }
}

/// 返回 false 表示连接已断开，会话应结束。
async fn handle_frame(data: Vec<u8>, upload: &mut Option<Upload>, sender: &mut Sender) -> bool {
    if let Some(state) = upload.as_mut() {
        state.received = state.received.saturating_add(data.len() as u64);
        if let Err(error) = state.file.write_all(&data).await {
            *upload = None;
            return send_error(sender, &error.to_string()).await;
        }
        if state.received >= state.size {
            let _ = state.file.shutdown().await;
            *upload = None;
            tracing::debug!("[FM] 文件接收完成");
            let payload = IoStreamData {
                data: NZUP.to_vec(),
            };
            return sender.send(payload).await.is_ok();
        }
        return true;
    }

    match data[0] {
        0 => list_dir(sender, path_from(&data[1..])).await,
        1 => download(sender, path_from(&data[1..])).await,
        2 => start_upload(&data[1..], upload, sender).await,
        other => tracing::debug!("[FM] 未知控制帧: {other}"),
    }
    true
}

fn path_from(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}

async fn send_error(sender: &mut Sender, message: &str) -> bool {
    let mut payload = NERR.to_vec();
    payload.extend_from_slice(message.as_bytes());
    sender.send(IoStreamData { data: payload }).await.is_ok()
}

/// 目录不存在时回退到 HOME（与 index.js 的 listDir 一致）。
async fn list_dir(sender: &mut Sender, dir: PathBuf) {
    if let Ok(payload) = read_dir_payload(&dir).await {
        let _ = sender.send(IoStreamData { data: payload }).await;
        return;
    }

    let home = home_dir();
    match read_dir_payload(&home).await {
        Ok(payload) => {
            let _ = sender.send(IoStreamData { data: payload }).await;
        }
        Err(error) => {
            let _ = send_error(sender, &error).await;
        }
    }
}

/// NZFN + 路径长度(BE32) + 路径 + [是否目录, 名称长度, 名称] ...
async fn read_dir_payload(dir: &Path) -> Result<Vec<u8>, String> {
    let mut payload = Vec::new();
    let path_bytes = dir.to_string_lossy().into_owned().into_bytes();
    payload.extend_from_slice(NZFN);
    payload.extend_from_slice(&(path_bytes.len() as u32).to_be_bytes());
    payload.extend_from_slice(&path_bytes);

    let mut entries = fs::read_dir(dir)
        .await
        .map_err(|error| format!("{dir:?}: {error}"))?;
    while let Some(entry) = entries.next_entry().await.map_err(|e| e.to_string())? {
        let is_dir = entry
            .file_type()
            .await
            .map(|file_type| file_type.is_dir())
            .unwrap_or(false);
        let name = entry.file_name();
        let name = name.to_string_lossy().into_owned();
        let name = name.into_bytes();
        payload.push(u8::from(is_dir));
        payload.push(name.len() as u8);
        payload.extend_from_slice(&name);
    }
    Ok(payload)
}

async fn download(sender: &mut Sender, path: PathBuf) {
    let metadata = match fs::metadata(&path).await {
        Ok(metadata) => metadata,
        Err(error) => {
            let _ = send_error(sender, &error.to_string()).await;
            return;
        }
    };
    if metadata.len() == 0 {
        let _ = send_error(sender, "requested file is empty").await;
        return;
    }

    let mut header = NZTD.to_vec();
    header.extend_from_slice(&metadata.len().to_be_bytes());
    if sender.send(IoStreamData { data: header }).await.is_err() {
        return;
    }

    let mut file = match fs::File::open(&path).await {
        Ok(file) => file,
        Err(error) => {
            let _ = send_error(sender, &error.to_string()).await;
            return;
        }
    };

    let mut buffer = vec![0u8; CHUNK_SIZE];
    loop {
        let size = match file.read(&mut buffer).await {
            Ok(size) => size,
            Err(error) => {
                let _ = send_error(sender, &error.to_string()).await;
                return;
            }
        };
        if size == 0 {
            return;
        }
        let payload = IoStreamData {
            data: buffer[..size].to_vec(),
        };
        if sender.send(payload).await.is_err() {
            return;
        }
    }
}

/// 上传头：文件大小(BE64) + 路径，后续帧为文件内容。
async fn start_upload(data: &[u8], upload: &mut Option<Upload>, sender: &mut Sender) {
    if data.len() < 8 {
        let _ = send_error(sender, "data is invalid").await;
        return;
    }

    let size = u64::from_be_bytes([
        data[0], data[1], data[2], data[3], data[4], data[5], data[6], data[7],
    ]);
    let path = path_from(&data[8..]);

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
        && let Err(error) = fs::create_dir_all(parent).await
    {
        let _ = send_error(sender, &error.to_string()).await;
        return;
    }

    match fs::File::create(&path).await {
        Ok(file) => {
            tracing::debug!("[FM] 接收文件: {} 大小: {size}", path.display());
            *upload = Some(Upload {
                file,
                size,
                received: 0,
            });
        }
        Err(error) => {
            let _ = send_error(sender, &error.to_string()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn encodes_directory_listing() {
        let dir = std::env::temp_dir().join(format!("nzws-fm-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("hello.txt"), b"hi")
            .await
            .unwrap();

        let payload = read_dir_payload(&dir).await.unwrap();
        assert_eq!(&payload[0..4], NZFN);
        let path_len =
            u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
        assert_eq!(
            String::from_utf8_lossy(&payload[8..8 + path_len]),
            dir.to_string_lossy()
        );

        let body = &payload[8 + path_len..];
        let is_dir = body[0] == 1;
        let name_len = body[1] as usize;
        let name = String::from_utf8_lossy(&body[2..2 + name_len]).into_owned();
        assert!(!is_dir);
        assert_eq!(name, "hello.txt");

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn missing_directory_reports_error() {
        let result = read_dir_payload(Path::new("/definitely/not/here")).await;
        assert!(result.is_err());
    }

    #[test]
    fn parses_fm_task() {
        let task: FmTask = serde_json::from_str(r#"{"StreamID":"7"}"#).unwrap();
        assert_eq!(task.stream_id, "7");
    }

    #[test]
    fn magic_markers_match_js() {
        assert_eq!(NZFN, &[0x4e, 0x5a, 0x46, 0x4e]);
        assert_eq!(NZTD, &[0x4e, 0x5a, 0x54, 0x44]);
        assert_eq!(NERR, &[0x4e, 0x45, 0x52, 0x52]);
        assert_eq!(NZUP, &[0x4e, 0x5a, 0x55, 0x50]);
    }
}
