use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataMap;
use tonic::transport::server::TcpIncoming;
use tonic::{Request, Response, Status};

use nzws_rust::agent;
use nzws_rust::config::Config;
use nzws_rust::proto::nezha_service_server::{NezhaService, NezhaServiceServer};
use nzws_rust::proto::{
    GeoIp, Host, IoStreamData, Receipt, State, Task, TaskResult, Uint64Receipt,
};

const AUTH_KEYS: [&str; 4] = [
    "client-secret",
    "client-uuid",
    "client_secret",
    "client_uuid",
];

#[derive(Default)]
struct Observed {
    auth: Mutex<Vec<String>>,
    hosts: AtomicUsize,
    states: AtomicUsize,
    geoip: AtomicUsize,
    stream_ids: Mutex<Vec<String>>,
    fm_listing: Mutex<Vec<u8>>,
    task_tx: Mutex<Option<mpsc::Sender<Result<Task, Status>>>>,
}

/// 最小可用的哪吒服务端，用来验证 agent 的上报与任务下发。
#[derive(Clone, Default)]
struct Mock(Arc<Observed>);

impl Mock {
    fn record_auth(&self, metadata: &MetadataMap) {
        let mut seen = Vec::new();
        for key in AUTH_KEYS {
            if let Some(value) = metadata.get(key) {
                seen.push(format!("{key}={}", value.to_str().unwrap_or("<binary>")));
            }
        }
        self.0.auth.lock().unwrap().push(seen.join("&"));
    }
}

#[tonic::async_trait]
impl NezhaService for Mock {
    type ReportSystemStateStream = ReceiverStream<Result<Receipt, Status>>;

    async fn report_system_state(
        &self,
        request: Request<tonic::Streaming<State>>,
    ) -> Result<Response<Self::ReportSystemStateStream>, Status> {
        self.record_auth(request.metadata());
        let mut states = request.into_inner();
        let (tx, rx) = mpsc::channel(8);
        let observed = Arc::clone(&self.0);

        tokio::spawn(async move {
            while let Ok(Some(state)) = states.message().await {
                assert!(state.uptime > 0, "uptime should be reported");
                observed.states.fetch_add(1, Ordering::SeqCst);
                let receipt = Ok(Receipt { proced: true });
                if tx.send(receipt).await.is_err() {
                    break;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn report_system_info(
        &self,
        request: Request<Host>,
    ) -> Result<Response<Receipt>, Status> {
        self.record_auth(request.metadata());
        Ok(Response::new(Receipt { proced: true }))
    }

    type RequestTaskStream = ReceiverStream<Result<Task, Status>>;

    async fn request_task(
        &self,
        request: Request<tonic::Streaming<TaskResult>>,
    ) -> Result<Response<Self::RequestTaskStream>, Status> {
        self.record_auth(request.metadata());
        // 消费服务端的 TaskResult，避免发送端被阻塞。
        let mut results = request.into_inner();
        tokio::spawn(async move { while let Ok(Some(_)) = results.message().await {} });

        let (tx, rx) = mpsc::channel(4);
        *self.0.task_tx.lock().unwrap() = Some(tx.clone());
        let task = Task {
            id: 7,
            r#type: 11,
            data: r#"{"StreamID":"mock-stream"}"#.to_string(),
        };
        tokio::spawn(async move {
            let _ = tx.send(Ok(task)).await;
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type IOStreamStream = ReceiverStream<Result<IoStreamData, Status>>;

    async fn io_stream(
        &self,
        request: Request<tonic::Streaming<IoStreamData>>,
    ) -> Result<Response<Self::IOStreamStream>, Status> {
        self.record_auth(request.metadata());
        let mut frames = request.into_inner();
        let (tx, rx) = mpsc::channel(8);
        let observed = Arc::clone(&self.0);

        tokio::spawn(async move {
            // 1. 会话握手：0xff 0x05 0xff 0x05 + StreamID
            let Ok(Some(first)) = frames.message().await else {
                return;
            };
            if first.data.starts_with(&[0xff, 0x05, 0xff, 0x05]) {
                let stream_id = String::from_utf8_lossy(&first.data[4..]).into_owned();
                observed.stream_ids.lock().unwrap().push(stream_id);
            }

            // 2. 请求列目录
            let mut request_frame = vec![0u8];
            request_frame.extend_from_slice(b"/tmp");
            let payload = Ok(IoStreamData {
                data: request_frame,
            });
            if tx.send(payload).await.is_err() {
                return;
            }

            // 3. 等待 NZFN 目录响应
            while let Ok(Some(frame)) = frames.message().await {
                if frame.data.starts_with(b"NZFN") {
                    *observed.fm_listing.lock().unwrap() = frame.data;
                    break;
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn report_geo_ip(&self, request: Request<GeoIp>) -> Result<Response<GeoIp>, Status> {
        self.record_auth(request.metadata());
        self.0.geoip.fetch_add(1, Ordering::SeqCst);
        Ok(Response::new(request.into_inner()))
    }

    async fn report_system_info2(
        &self,
        request: Request<Host>,
    ) -> Result<Response<Uint64Receipt>, Status> {
        self.record_auth(request.metadata());
        let host = request.into_inner();
        assert!(!host.cpu.is_empty(), "cpu info should be reported");
        assert!(!host.version.is_empty());
        self.0.hosts.fetch_add(1, Ordering::SeqCst);
        Ok(Response::new(Uint64Receipt {
            data: 1_700_000_000,
        }))
    }
}

async fn spawn_mock_server(mock: Mock) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let incoming = TcpIncoming::from(listener).with_nodelay(Some(true));
    tokio::spawn(async move {
        let server = tonic::transport::Server::builder().add_service(NezhaServiceServer::new(mock));
        let _ = server.serve_with_incoming(incoming).await;
    });
    port
}

async fn wait_until(predicate: impl Fn() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    predicate()
}

#[tokio::test]
async fn agent_reports_and_receives_tasks() {
    let mock = Mock::default();
    let observed = Arc::clone(&mock.0);
    let port = spawn_mock_server(mock).await;

    let mut config = Config::from_env();
    config.nezha_server = format!("127.0.0.1:{port}");
    config.nezha_key = "test-key".to_string();
    config.uuid = "test-uuid".to_string();

    let agent = tokio::spawn(agent::run(Arc::new(config)));

    // 主机信息 + 至少两次状态上报（采样间隔 4s）+ 一次文件管理会话
    let settled = wait_until(
        || {
            observed.hosts.load(Ordering::SeqCst) >= 1
                && observed.states.load(Ordering::SeqCst) >= 2
                && !observed.fm_listing.lock().unwrap().is_empty()
        },
        Duration::from_secs(60),
    )
    .await;

    agent.abort();

    assert!(settled, "agent did not complete a full report cycle");
    assert!(
        observed.geoip.load(Ordering::SeqCst) >= 1,
        "geoip should be reported"
    );

    let auth = observed.auth.lock().unwrap();
    assert!(!auth.is_empty(), "every rpc should carry auth metadata");
    for entry in auth.iter() {
        for key in AUTH_KEYS {
            assert!(
                entry.contains(&format!("{key}=")),
                "missing {key} in {entry}"
            );
        }
    }

    assert!(
        observed
            .stream_ids
            .lock()
            .unwrap()
            .contains(&"mock-stream".to_string())
    );

    let listing = observed.fm_listing.lock().unwrap().clone();
    let path_len = u32::from_be_bytes([listing[4], listing[5], listing[6], listing[7]]) as usize;
    assert_eq!(&listing[8..8 + path_len], b"/tmp");

    // 释放任务发送端，避免 mock 服务端一直持有连接
    observed.task_tx.lock().unwrap().take();
}
