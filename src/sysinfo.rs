use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::AGENT_VERSION;
use crate::proto::{Host, State, StateSensorTemperature};

const EXCLUDE_INTERFACES: [&str; 12] = [
    "lo",
    "tun",
    "docker",
    "veth",
    "br-",
    "vmbr",
    "vnet",
    "kube",
    "Meta",
    "tailscale",
    "fw",
    "tap",
];

const EXPECT_FS_TYPES: [&str; 17] = [
    "apfs",
    "ext4",
    "ext3",
    "ext2",
    "f2fs",
    "reiserfs",
    "jfs",
    "bcachefs",
    "btrfs",
    "fuseblk",
    "zfs",
    "simfs",
    "ntfs",
    "fat32",
    "exfat",
    "xfs",
    "fuse.rclone",
];

/// 上一次采样值，用于计算 CPU 占用率与网速。
#[derive(Debug, Default, Clone)]
pub struct Counters {
    cpu: Option<(u64, u64)>,
    net: Option<(u64, u64, u64)>,
    net_in: u64,
    net_out: u64,
    net_in_speed: u64,
    net_out_speed: u64,
}

fn read_file(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn should_exclude_interface(name: &str) -> bool {
    EXCLUDE_INTERFACES
        .iter()
        .any(|excluded| name.contains(excluded))
}

fn is_expected_fs(fs_type: &str) -> bool {
    EXPECT_FS_TYPES.contains(&fs_type.to_lowercase().as_str())
}

/// /proc/meminfo -> key/value
fn meminfo() -> HashMap<String, u64> {
    let mut values = HashMap::new();
    for line in read_file("/proc/meminfo").lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let kb: u64 = rest
            .split_whitespace()
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or_default();
        values.insert(key.to_string(), kb * 1024);
    }
    values
}

/// /proc/stat 的聚合行 -> (total, idle)
fn cpu_ticks() -> Option<(u64, u64)> {
    let line = read_file("/proc/stat")
        .lines()
        .find(|line| line.starts_with("cpu "))
        .map(|line| line.to_string())?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|value| value.parse::<u64>().ok())
        .collect();
    if values.len() < 4 {
        return None;
    }
    let total: u64 = values.iter().sum();
    let idle = values[3] + values.get(4).copied().unwrap_or_default();
    Some((total, idle))
}

/// 网卡累计收发字节（排除虚拟网卡）。
fn net_transfer() -> (u64, u64) {
    let mut rx = 0u64;
    let mut tx = 0u64;
    let Ok(entries) = fs::read_dir("/sys/class/net") else {
        return (0, 0);
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if should_exclude_interface(&name) {
            continue;
        }
        let base = format!("/sys/class/net/{name}/statistics");
        rx += read_file(&format!("{base}/rx_bytes"))
            .trim()
            .parse::<u64>()
            .unwrap_or_default();
        tx += read_file(&format!("{base}/tx_bytes"))
            .trim()
            .parse::<u64>()
            .unwrap_or_default();
    }
    (rx, tx)
}

/// 挂载点统计（只统计真实文件系统），返回 (总量, 已用)。
fn disk_usage() -> (u64, u64) {
    let mut seen = HashSet::new();
    let mut total = 0u64;
    let mut used = 0u64;
    for line in read_file("/proc/mounts").lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 3 || !is_expected_fs(fields[2]) {
            continue;
        }
        let (device, mount_point) = (fields[0], fields[1]);
        if !seen.insert(device.to_string()) {
            continue;
        }
        if let Some((size, used_bytes)) = statvfs(mount_point) {
            total += size;
            used += used_bytes;
        }
    }
    (total, used)
}

fn statvfs(mount_point: &str) -> Option<(u64, u64)> {
    let path = CString::new(mount_point.replace("\\040", " ")).ok()?;
    let mut stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let frsize = stat.f_frsize as u64;
    let blocks = stat.f_blocks as u64;
    let available = stat.f_bavail as u64;
    Some((blocks * frsize, (blocks - available) * frsize))
}

/// /proc/net/tcp{,6} 与 /proc/net/udp{,6} 的连接数。
fn conn_count() -> (u64, u64) {
    let count = |path: &str| -> u64 {
        let content = read_file(path);
        if content.is_empty() {
            0
        } else {
            content.lines().count().saturating_sub(2) as u64
        }
    };
    let tcp = count("/proc/net/tcp") + count("/proc/net/tcp6");
    let udp = count("/proc/net/udp") + count("/proc/net/udp6");
    (tcp, udp)
}

fn process_count() -> u64 {
    let Ok(entries) = fs::read_dir("/proc") else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            !name.is_empty() && name.chars().all(|c| c.is_ascii_digit())
        })
        .count() as u64
}

fn uptime_secs() -> u64 {
    read_file("/proc/uptime")
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<f64>().ok())
        .map(|value| value as u64)
        .unwrap_or_default()
}

fn load_avg() -> (f64, f64, f64) {
    let fields: Vec<f64> = read_file("/proc/loadavg")
        .split_whitespace()
        .take(3)
        .filter_map(|value| value.parse().ok())
        .collect();
    (
        fields.first().copied().unwrap_or_default(),
        fields.get(1).copied().unwrap_or_default(),
        fields.get(2).copied().unwrap_or_default(),
    )
}

/// /etc/os-release -> (platform, version)
fn os_release() -> (String, String) {
    let mut platform = String::new();
    let mut version = String::new();
    for line in read_file("/etc/os-release").lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim_matches('"').to_string();
        match key {
            "PRETTY_NAME" => platform = value,
            "VERSION_ID" => version = value,
            _ => {}
        }
    }
    (platform, version)
}

/// /proc/cpuinfo -> (型号描述, 物理核心数)
fn cpu_info() -> (String, usize) {
    let content = read_file("/proc/cpuinfo");
    let mut vendor = String::new();
    let mut brand = String::new();
    let mut logical = 0usize;
    let mut physical: HashSet<(String, String)> = HashSet::new();

    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "processor" => logical += 1,
            "vendor_id" if vendor.is_empty() => vendor = value.to_string(),
            "model name" if brand.is_empty() => brand = value.to_string(),
            "physical id" | "core id" => {}
            _ => {}
        }
    }

    let mut current_socket = String::new();
    for line in content.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "physical id" => current_socket = value.trim().to_string(),
            "core id" => {
                physical.insert((current_socket.clone(), value.trim().to_string()));
            }
            _ => {}
        }
    }

    let cores = if physical.is_empty() {
        logical
    } else {
        physical.len()
    };
    let description = format!("{vendor} {brand} {cores} Physical Core");
    (description, cores)
}

fn arch() -> String {
    match std::env::consts::ARCH {
        "x86" => "i386".to_string(),
        value => value.to_string(),
    }
}

/// 上报主机信息（对应 index.js 的 getHost）。
pub fn host_info() -> Host {
    let memory = meminfo();
    let (description, _) = cpu_info();
    let (platform, platform_version) = os_release();
    let (disk_total, _) = disk_usage();

    Host {
        platform: if platform.is_empty() {
            std::env::consts::OS.to_string()
        } else {
            platform
        },
        platform_version,
        cpu: vec![description],
        mem_total: memory.get("MemTotal").copied().unwrap_or_default(),
        disk_total,
        swap_total: memory.get("SwapTotal").copied().unwrap_or_default(),
        arch: arch(),
        virtualization: String::new(),
        boot_time: now_secs().saturating_sub(uptime_secs()),
        version: AGENT_VERSION.to_string(),
        gpu: Vec::new(),
    }
}

/// 采样一次运行状态（对应 index.js 的 trackNetworkSpeed + getState）。
pub fn sample(counters: &mut Counters) -> State {
    let memory = meminfo();
    let mem_total = memory.get("MemTotal").copied().unwrap_or_default();
    let mem_available = memory
        .get("MemAvailable")
        .or_else(|| memory.get("MemFree"))
        .copied()
        .unwrap_or_default();
    let swap_total = memory.get("SwapTotal").copied().unwrap_or_default();
    let swap_free = memory.get("SwapFree").copied().unwrap_or_default();

    let ticks = cpu_ticks();
    let cpu = match (ticks, counters.cpu) {
        (Some(current), Some((previous_total, previous_idle))) => {
            let total_delta = current.0.saturating_sub(previous_total) as f64;
            let idle_delta = current.1.saturating_sub(previous_idle) as f64;
            if total_delta > 0.0 {
                (100.0 - (idle_delta / total_delta) * 100.0).max(0.0)
            } else {
                0.0
            }
        }
        _ => 0.0,
    };
    counters.cpu = ticks;

    let (rx, tx) = net_transfer();
    let now = now_secs();
    if let Some((_, _, previous_time)) = counters.net {
        let elapsed = now.saturating_sub(previous_time).max(1);
        counters.net_in_speed = rx.saturating_sub(counters.net_in) / elapsed;
        counters.net_out_speed = tx.saturating_sub(counters.net_out) / elapsed;
    }
    counters.net_in = rx;
    counters.net_out = tx;
    counters.net = Some((rx, tx, now));

    let (_, disk_used) = disk_usage();
    let (load1, load5, load15) = load_avg();
    let (tcp_conn_count, udp_conn_count) = conn_count();

    State {
        cpu,
        mem_used: mem_total.saturating_sub(mem_available),
        swap_used: swap_total.saturating_sub(swap_free),
        disk_used,
        net_in_transfer: rx,
        net_out_transfer: tx,
        net_in_speed: counters.net_in_speed,
        net_out_speed: counters.net_out_speed,
        uptime: uptime_secs(),
        load1,
        load5,
        load15,
        tcp_conn_count,
        udp_conn_count,
        process_count: process_count(),
        temperatures: Vec::<StateSensorTemperature>::new(),
        gpu: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_virtual_interfaces() {
        assert!(should_exclude_interface("docker0"));
        assert!(should_exclude_interface("veth1234"));
        assert!(should_exclude_interface("lo"));
        assert!(!should_exclude_interface("eth0"));
        assert!(!should_exclude_interface("ens3"));
    }

    #[test]
    fn filters_filesystem_types() {
        assert!(is_expected_fs("ext4"));
        assert!(is_expected_fs("EXT4"));
        assert!(!is_expected_fs("overlay"));
        assert!(!is_expected_fs("proc"));
    }

    #[test]
    fn samples_without_panicking() {
        let mut counters = Counters::default();
        let first = sample(&mut counters);
        let second = sample(&mut counters);
        assert!(second.cpu >= 0.0 && second.cpu <= 100.0);
        assert!(first.uptime > 0, "uptime should be readable on linux");
    }

    #[test]
    fn builds_host_info() {
        let host = host_info();
        assert!(!host.platform.is_empty());
        assert_eq!(host.version, AGENT_VERSION);
        assert!(!host.cpu.is_empty());
        assert!(!host.arch.is_empty());
    }
}
