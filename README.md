# nzws-rust

`index.js`（nzws-js）的 Rust 实现：同一个进程里同时提供

1. **伪装页 + 订阅**：`GET /` 返回伪装页，`GET /{SUB_PATH}` 返回 base64 订阅（VLESS + Trojan + SS/v2ray-plugin）；
2. **WebSocket 代理**：`/{WSPATH}` 上解析 VLESS / Trojan / SS 首包，DoH 解析目标域名后桥接到上游 TCP；
3. **哪吒 (Nezha) agent**：gRPC 上报主机信息、运行状态、GeoIP，并通过 `IOStream` 提供终端（type 8）与文件管理（type 11）。

## 构建与运行

```bash
# 需要 rustc/cargo 1.85+（edition 2024）；protoc 由 protoc-bin-vendored 内置，无需系统安装
cargo build --release
UUID=xxxx NEZHA_SERVER=dashboard.example.com:443 NEZHA_KEY=yyy ./target/release/nzws-rust
```

```bash
cargo test          # 39 个测试：握手解析 / 订阅生成 / 系统采集 + 端到端代理
cargo clippy        # 无告警
```

Docker：

```bash
docker build -t nzws-rust .
docker run -p 3000:3000 -e UUID=xxxx -e NEZHA_KEY=yyy -e NEZHA_SERVER=xxx:443 nzws-rust
```

## 部署到 Vercel

Vercel 从 2025-12 起提供官方 Rust 运行时（Beta），本项目直接按它支持的
**standalone server** 形态构建：`src/main.rs` 监听 `$PORT`，运行时外层的 IPC 代理
负责协议转换和 HTTP `101` 升级隧道，因此 **不需要 `vercel_runtime`，也不需要任何
Vercel 专用代码**，WebSocket 代理与后台哪吒 agent 都能照常工作。

参考实现：`vercel/vercel` 的 `packages/rust/test/fixtures/02-standalone-axum`
（纯 axum + `$PORT`）与 `internals/ipc-proxy/bootstrap/proxy.go`（显式处理协议升级）。

步骤：

1. 导入仓库后把 **Root Directory 设为 `rust`**（仓库根目录是 JS 版本的 `vercel.json`）。
2. `rust/vercel.json` 已写好 `framework: "rust"`、`fluid: true`（WebSocket 必需）、
   `regions: ["sin1"]` 和 `maxDuration: "max"`，无需再改。
3. 在 Vercel 的 Environment Variables 里配置 `UUID` / `DOMAIN` / `NEZHA_*`（见下表），
   不要把密钥写进仓库。
4. `vercel deploy`（或推送 Git 触发自动部署）。本地联调用 `vercel dev`，
   它会注入 `PORT` 并复用同一个二进制。

已知限制（都来自 Vercel Functions 本身的边界，不是本项目的问题）：

- **连接时长**：WebSocket 连接会在函数 `maxDuration` 到期时被切断。Hobby 上限 300s，
  Pro 默认 800s / 可申请 1800s。客户端必须实现自动重连。
- **实例不持久**：实例会被冻结、扩容或缩容到 0，内存状态不可依赖。哪吒 agent 与
  保活任务都带重试，实例恢复后会自动重连；但实例休眠期间不会上报主机信息。
- **构建依赖**：`protoc` 已内置（`protoc-bin-vendored`），构建机无需安装。
  `aws-lc-rs`（reqwest/tonic 的 TLS 后端）需要一个 C 编译器；Vercel 的构建镜像是
  Amazon Linux 2023，预装包清单未公开，万一构建报缺 `cc`，在 `vercel.json` 里加
  `"installCommand": "dnf install -y gcc make cmake"` 即可。
- **信任库**：HTTP 侧走系统根证书，哪吒 gRPC 侧走 webpki-roots，详见「与 JS 版本的差异」。

> 官方 Rust 模板还有另一种形态：`api/*.rs` + `vercel_runtime`（`VercelLayer` +
> `vercel_runtime::run`），见 `vercel/examples/rust/{axum,websocket,wait-until}`。
> 本项目没有采用，因为那种形态每次请求都是短生命周期的 handler，常驻的哪吒 agent
> 和保活循环无法运行；如果只需要订阅页 + 代理，改写成那种形态也完全可行。

## 环境变量

与 `index.js` 完全同名，默认值也一致。

| 变量 | 默认值 | 说明 |
|---|---|---|
| `UUID` | `d1cf4b9c-...` | 节点 UUID（VLESS id / Trojan 密码 / SS 密码） |
| `WSPATH` | `UUID` 前 8 位 | WebSocket 代理路径前缀 |
| `SUB_PATH` | `vercel` | 订阅路径 |
| `NAME` | `Vercel` | 节点名称前缀 |
| `DOMAIN` | `your-domain.com` | 订阅里的接入域名；留空或占位值时自动探测公网 IP |
| `PORT` | `3000` | 监听端口 |
| `NEZHA_SERVER` | 空 | 哪吒 gRPC 地址，空则不启动 agent |
| `NEZHA_KEY` | 空 | 哪吒 agent 密钥 |
| `AUTO_ACCESS` | `false` | 向 serv00 保活服务登记订阅地址 |
| `SHOW_LOG` | `false` | 打开后输出 debug 级日志 |
| `RUST_LOG` | 自动 | 直接指定 tracing 过滤级别，优先级高于 `SHOW_LOG` |

## 代码结构

| 文件 | 对应 index.js 的部分 |
|---|---|
| `src/config.rs` | 环境变量、TLS 端口表、测速域名黑名单 |
| `src/web.rs` | `http.createServer` 路由（伪装页 / 订阅 / 404 / WS 升级） |
| `src/subscription.rs` | `getisp` / `getip` / 三条订阅链接 |
| `src/proxy/handshake.rs` | `handleVlsConnection` / `handleTrojConnection` / `handleSsConnection` 的报文解析 |
| `src/proxy/bridge.rs` | `createWebSocketStream`（WebSocket ↔ AsyncRead/AsyncWrite） |
| `src/proxy/mod.rs` | `wss.on('connection')` 分派与 TCP 桥接 |
| `src/dns.rs` | `resolveHost`（dns.google DoH） |
| `src/geoip.rs` | `fetchIP`（公网 v4/v6 探测） |
| `src/sysinfo.rs` | `getHost` / `getState` / `trackNetworkSpeed`（直接读 `/proc`、`/sys`） |
| `src/agent/mod.rs` | `startNezhaAgent` 主循环、gRPC 认证、GeoIP 上报 |
| `src/agent/terminal.rs` `src/agent/pty.rs` | `handleTerminalTask`（node-pty → portable-pty） |
| `src/agent/filemanager.rs` | `handleFMTask`（NZFN/NZTD/NERR/NZUP 协议） |
| `src/keepalive.rs` | `addAccessTask` |
| `proto/nezha.proto` | `PROTO_CONTENT` |

## 与 JS 版本的差异

- **伪装页**：`index.html` 编译时内嵌；运行时工作目录下的 `index.html` 优先，因此仍可以不重新编译就换页面。
- **系统指标**：直接读 `/proc`、`/sys` 与 `statvfs`，完整指标只在 Linux 下可用（Vercel/容器环境即 Linux）；温度与 GPU 和 JS 版一样上报空值。
- **终端**：优先 portable-pty，失败时依次降级到 `script -qfc "$SHELL -i" /dev/null`、`python3`、`python`，与 node-pty 的降级链一致。
- **DoH**：`DNS_SERVERS` 现在真正生效（`dns.google` 的 `server=` 参数）；JS 里两个 server 实际请求的是同一个地址。
- **布尔量**：`AUTO_ACCESS` / `SHOW_LOG` 把 `false` / `0` / `off` 识别为关闭；JS 里非空字符串一律为真。
- **WebSocket**：单条消息上限放宽到 4 MB（axum 默认 64 KB），避免大帧被断开。
- **TLS**：哪吒 gRPC 连接使用 webpki-roots 内置根证书校验，自签名证书需要自行追加根证书；订阅/GeoIP/保活用的 HTTP 客户端（reqwest 0.13）改用系统信任库，容器内需安装 `ca-certificates`（Dockerfile 已包含）。
- **部署**：Vercel 现在有官方 Rust 运行时（Beta），Rust 版可以直接部署（Root Directory 指向 `rust`），见上一节；根目录的 `vercel.json` 仍然只服务于 JS 版本。Docker / 自有 VPS 同样是受支持的目标。
