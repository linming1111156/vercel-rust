//! nzws-rust：index.js (nzws-js) 的 Rust 实现。
//! VLESS / Trojan / SS WebSocket 代理 + 哪吒 (Nezha) gRPC agent。

// tonic 生成的客户端返回的 `Status` 体积偏大，属于依赖自身的特征。
#![allow(clippy::result_large_err)]

pub mod agent;
pub mod app;
pub mod config;
pub mod dns;
pub mod geoip;
pub mod keepalive;
pub mod net;
pub mod proxy;
pub mod subscription;
pub mod sysinfo;
pub mod web;

pub mod proto {
    tonic::include_proto!("proto");
}
