pub mod filemanager;
pub mod pty;
pub mod terminal;

use std::error::Error as StdError;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval, sleep, timeout};
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::{MetadataMap, MetadataValue};
use tonic::service::interceptor::{InterceptedService, Interceptor};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Request, Status};

use crate::config::{
    Config, HOST_REPORT_PERIOD_SECS, IP_REPORT_PERIOD_SECS, NETWORK_TIMEOUT_SECS,
    REPORT_DELAY_SECS, RETRY_DELAY_SECS, should_use_tls,
};
use crate::geoip;
use crate::proto::{
    GeoIp, IoStreamData, Ip, Receipt, State, TaskResult, nezha_service_client::NezhaServiceClient,
};
use crate::sysinfo;

pub const TASK_TERMINAL: u64 = 8;
pub const TASK_FM: u64 = 11;

const GEOIP_TIMEOUT: Duration = Duration::from_secs(15);
const HOST_REFRESH_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

type Client = NezhaServiceClient<InterceptedService<Channel, MetadataInterceptor>>;
type BoxError = Box<dyn StdError + Send + Sync>;
pub type IoStream = (
    mpsc::Sender<IoStreamData>,
    tonic::codec::Streaming<IoStreamData>,
);

/// 每个 gRPC 请求都带上认证头（新旧两套 key，兼容不同版本的哪吒服务端）。
#[derive(Clone)]
pub(crate) struct MetadataInterceptor {
    metadata: MetadataMap,
}

impl Interceptor for MetadataInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        *request.metadata_mut() = self.metadata.clone();
        Ok(request)
    }
}

fn build_metadata(config: &Config) -> MetadataMap {
    let mut metadata = MetadataMap::new();
    let entries = [
        ("client-secret", &config.nezha_key),
        ("client-uuid", &config.uuid),
        ("client_secret", &config.nezha_key),
        ("client_uuid", &config.uuid),
    ];
    for (key, value) in entries {
        if let Ok(value) = MetadataValue::try_from(value.as_str()) {
            metadata.insert(key, value);
        }
    }
    metadata
}

async fn connect(config: &Config) -> Result<Client, BoxError> {
    let server = config.nezha_server.as_str();
    let use_tls = should_use_tls(server);
    let scheme = if use_tls { "https" } else { "http" };
    let mut endpoint = Endpoint::from_shared(format!("{scheme}://{server}"))?
        .connect_timeout(Duration::from_secs(NETWORK_TIMEOUT_SECS))
        .tcp_nodelay(true);

    if use_tls {
        let domain = server.split(':').next().unwrap_or(server);
        endpoint = endpoint.tls_config(ClientTlsConfig::new().domain_name(domain))?;
    }

    let channel = endpoint.connect().await?;
    let service = InterceptedService::new(
        channel,
        MetadataInterceptor {
            metadata: build_metadata(config),
        },
    );
    Ok(NezhaServiceClient::new(service))
}

/// 哪吒 agent 主循环：断线后按 RETRY_DELAY 重连。
pub async fn run(config: Arc<Config>) {
    if config.nezha_server.is_empty() || config.nezha_key.is_empty() {
        tracing::info!("[Nezha] NEZHA_SERVER 或 NEZHA_KEY 未配置，跳过哪吒 agent");
        return;
    }

    tracing::debug!("[Nezha] 服务器: {}", config.nezha_server);
    tracing::debug!(
        "[Nezha] TLS: {}",
        if should_use_tls(&config.nezha_server) {
            "启用"
        } else {
            "禁用"
        }
    );
    tracing::debug!("[Nezha] UUID: {}", config.uuid);

    let streams = Arc::new(AtomicUsize::new(0));
    let mut agent = Agent {
        config,
        streams,
        last_reported_ip: None,
    };

    loop {
        if let Err(error) = agent.worker().await {
            tracing::debug!("[Agent] connect error: {error}");
        }
        drain_streams(&agent.streams).await;
        tracing::info!("[Agent] retry connect...");
        sleep(Duration::from_millis(RETRY_DELAY_SECS)).await;
    }
}

struct Agent {
    config: Arc<Config>,
    streams: Arc<AtomicUsize>,
    last_reported_ip: Option<String>,
}

impl Agent {
    async fn worker(&mut self) -> Result<(), BoxError> {
        let mut client = connect(&self.config).await?;
        tracing::info!("nzbot is running...");

        let host = sysinfo::host_info();
        let receipt = timeout(NETWORK_TIMEOUT, client.report_system_info2(host))
            .await
            .map_err(|_| Status::deadline_exceeded("ReportSystemInfo2 timeout"))?
            .map_err(|status| {
                tracing::error!("[Agent] 上报系统信息失败: {status}");
                Box::<dyn StdError + Send + Sync>::from(status.to_string())
            })?;
        let dashboard_boot_time = receipt.into_inner().data;
        tracing::debug!("[Agent] 上报系统信息成功, Dashboard BootTime: {dashboard_boot_time}");

        let mut geoip_reported = self.report_geo_ip(&mut client, true).await;
        if !geoip_reported {
            tracing::debug!("[GeoIP] 首次上报异常");
        }

        // 任务流：只接收任务，不回传 TaskResult，但需要保持发送端存活。
        let (result_tx, result_rx) = mpsc::channel::<TaskResult>(8);
        let mut tasks = client
            .request_task(ReceiverStream::new(result_rx))
            .await?
            .into_inner();
        tracing::debug!("[Agent] RequestTask strem connect");

        // 状态流：发送采样 -> 等待 Receipt。
        let (state_tx, state_rx) = mpsc::channel::<State>(4);
        let mut receipts = client
            .report_system_state(ReceiverStream::new(state_rx))
            .await?
            .into_inner();
        tracing::debug!("[Agent] ReportSystemState strem connect");

        let mut ticker = interval(Duration::from_secs(REPORT_DELAY_SECS));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ticker.tick().await;

        let mut counters = sysinfo::Counters::default();
        let mut last_host_report = Instant::now();
        let mut last_ip_report = Instant::now();

        loop {
            tokio::select! {
                task = tasks.message() => match task {
                    Ok(Some(task)) => self.spawn_task(&client, task.r#type, task.data),
                    Ok(None) => {
                        tracing::debug!("[Agent] RequestTask strem finished");
                        break;
                    }
                    Err(status) => {
                        tracing::error!("[Agent] RequestTask strem error: {status}");
                        break;
                    }
                },
                _ = ticker.tick() => {
                    let outcome = self
                        .report_state(
                            &mut client,
                            &mut counters,
                            &state_tx,
                            &mut receipts,
                            &mut last_host_report,
                            &mut last_ip_report,
                            &mut geoip_reported,
                        )
                        .await;
                    if let Err(error) = outcome {
                        tracing::error!("[Agent] error: {error}");
                        break;
                    }
                }
            }
        }

        drop(state_tx);
        drop(result_tx);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn report_state(
        &mut self,
        client: &mut Client,
        counters: &mut sysinfo::Counters,
        state_tx: &mpsc::Sender<State>,
        receipts: &mut tonic::codec::Streaming<Receipt>,
        last_host_report: &mut Instant,
        last_ip_report: &mut Instant,
        geoip_reported: &mut bool,
    ) -> Result<(), BoxError> {
        let state = sysinfo::sample(counters);
        state_tx
            .send(state)
            .await
            .map_err(|_| Status::unavailable("state stream closed"))?;

        timeout(NETWORK_TIMEOUT, receipts.message())
            .await
            .map_err(|_| Status::deadline_exceeded("receipt timeout"))?
            .map_err(|status| -> BoxError { Box::from(status.to_string()) })?
            .ok_or_else(|| -> BoxError { Box::from("receipt stream closed") })?;

        if last_host_report.elapsed() >= Duration::from_secs(HOST_REPORT_PERIOD_SECS) {
            *last_host_report = Instant::now();
            let host = sysinfo::host_info();
            match timeout(HOST_REFRESH_TIMEOUT, client.report_system_info2(host)).await {
                Ok(Err(status)) => tracing::debug!("[Agent] 刷新主机信息失败: {status}"),
                Err(_) => tracing::debug!("[Agent] 刷新主机信息超时"),
                Ok(Ok(_)) => {}
            }
        }

        if !*geoip_reported
            || last_ip_report.elapsed() >= Duration::from_secs(IP_REPORT_PERIOD_SECS)
        {
            let force = !*geoip_reported;
            if self.report_geo_ip(client, force).await {
                *last_ip_report = Instant::now();
                *geoip_reported = true;
            }
        }

        Ok(())
    }

    async fn report_geo_ip(&mut self, client: &mut Client, force_update: bool) -> bool {
        let (ipv4, ipv6) = geoip::fetch_public_ips().await;
        let current = if ipv4.is_empty() {
            ipv6.clone()
        } else {
            ipv4.clone()
        };

        if !force_update && self.last_reported_ip.as_deref() == Some(current.as_str()) {
            return true;
        }

        if current.is_empty() {
            tracing::debug!("[GeoIP] 外部 IP 获取失败，发送空 IP（服务端将使用连接地址）");
        } else {
            tracing::debug!("[GeoIP] 获取到 IP: {current} 强制更新: {force_update}");
        }

        let body = GeoIp {
            use6: false,
            ip: Some(Ip { ipv4, ipv6 }),
            country_code: String::new(),
            dashboard_boot_time: 0,
        };

        match timeout(GEOIP_TIMEOUT, client.report_geo_ip(body)).await {
            Ok(Ok(_)) => {
                self.last_reported_ip = Some(current.clone());
                let shown = if current.is_empty() {
                    "(空，使用连接地址)"
                } else {
                    &current
                };
                tracing::debug!("[GeoIP] 上报成功, IP: {shown}");
                true
            }
            Ok(Err(status)) => {
                tracing::error!("[GeoIP] 上报失败: {status}");
                false
            }
            Err(_) => {
                tracing::error!("[GeoIP] RPC 超时");
                false
            }
        }
    }

    /// 终端 / 文件管理任务在独立的 IOStream 会话中执行。
    fn spawn_task(&self, client: &Client, task_type: u64, data: String) {
        if !matches!(task_type, TASK_TERMINAL | TASK_FM) {
            tracing::debug!("[Agent] 忽略任务类型: {task_type}");
            return;
        }

        self.streams.fetch_add(1, Ordering::Relaxed);
        let mut client = client.clone();
        let streams = Arc::clone(&self.streams);

        tokio::spawn(async move {
            match open_io_stream(&mut client).await {
                Ok(stream) => match task_type {
                    TASK_TERMINAL => terminal::run(data, stream).await,
                    _ => filemanager::run(data, stream).await,
                },
                Err(status) => tracing::error!("[Agent] IOStream 打开失败: {status}"),
            }
            streams.fetch_sub(1, Ordering::Relaxed);
        });
    }
}

const NETWORK_TIMEOUT: Duration = Duration::from_secs(NETWORK_TIMEOUT_SECS);

pub(crate) async fn open_io_stream(client: &mut Client) -> Result<IoStream, Status> {
    let (sender, receiver) = mpsc::channel::<IoStreamData>(32);
    let inbound = client
        .io_stream(ReceiverStream::new(receiver))
        .await?
        .into_inner();
    Ok((sender, inbound))
}

/// 会话握手：0xff 0x05 0xff 0x05 + StreamID。
pub(crate) fn stream_id_frame(stream_id: &str) -> Vec<u8> {
    let mut frame = vec![0xff, 0x05, 0xff, 0x05];
    frame.extend_from_slice(stream_id.as_bytes());
    frame
}

async fn drain_streams(streams: &AtomicUsize) {
    let active = streams.load(Ordering::Relaxed);
    if active == 0 {
        return;
    }
    tracing::debug!("[Agent] 等待 {active} 个活跃 IOStream 会话结束...");
    let deadline = Instant::now() + STREAM_DRAIN_TIMEOUT;
    while streams.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
        sleep(Duration::from_millis(100)).await;
    }
    let remaining = streams.load(Ordering::Relaxed);
    if remaining > 0 {
        tracing::warn!("[Agent] 仍有 {remaining} 个 IOStream 会话未结束，强制关闭");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_auth_metadata() {
        let mut config = Config::from_env();
        config.uuid = "abc".to_string();
        config.nezha_key = "key".to_string();

        let metadata = build_metadata(&config);
        assert_eq!(
            metadata.get("client-secret").unwrap().to_str().unwrap(),
            "key"
        );
        assert_eq!(
            metadata.get("client_uuid").unwrap().to_str().unwrap(),
            "abc"
        );
    }

    #[test]
    fn builds_stream_handshake() {
        assert_eq!(
            stream_id_frame("42"),
            vec![0xff, 0x05, 0xff, 0x05, b'4', b'2']
        );
        assert_eq!(stream_id_frame(""), vec![0xff, 0x05, 0xff, 0x05]);
    }
}
