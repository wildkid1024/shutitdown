use chrono::Local;
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{transport::Server, Request, Response, Status};

use node_agent::node_agent_server::{NodeAgent, NodeAgentServer};
use node_agent::*;

pub mod node_agent {
    tonic::include_proto!("node_agent");
}

// ──────────────────────────────────────
// Agent 状态
// ──────────────────────────────────────

#[derive(Debug, Clone)]
struct AgentState {
    version: &'static str,
    start_time: std::time::Instant,
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            start_time: std::time::Instant::now(),
        }
    }
}

// ──────────────────────────────────────
// Agent 服务实现
// ──────────────────────────────────────

#[derive(Debug, Default)]
pub struct NodeAgentService {
    state: Arc<std::sync::Mutex<AgentState>>,
}

impl NodeAgentService {
    pub fn new() -> Self {
        Self {
            state: Arc::new(std::sync::Mutex::new(AgentState::default())),
        }
    }

    /// 执行 shell 命令并返回输出
    fn run_command(cmd: &str) -> Result<(i32, String, String), String> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .output()
            .map_err(|e| format!("执行命令失败: {}", e))?;

        Ok((
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }

    /// 获取单行字段（从 key=value 形式提取）
    fn read_line(path: &str) -> String {
        fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    /// 采集 CPU 信息
    fn collect_cpu() -> (String, String) {
        let model = Self::read_line("/proc/cpuinfo")
            .lines()
            .find(|l| l.starts_with("model name"))
            .map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())
            .unwrap_or_else(|| "N/A".to_string());

        let cores = Self::read_line("/proc/cpuinfo")
            .lines()
            .filter(|l| l.starts_with("processor"))
            .count()
            .to_string();

        (model, cores)
    }

    /// 采集内存信息
    fn collect_memory() -> MemoryInfo {
        let total_kb = Self::read_line("/proc/meminfo")
            .lines()
            .find(|l| l.starts_with("MemTotal:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        let avail_kb = Self::read_line("/proc/meminfo")
            .lines()
            .find(|l| l.starts_with("MemAvailable:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        let used_kb = total_kb - avail_kb;
        let pct = if total_kb > 0.0 {
            format!("{:.0}", used_kb / total_kb * 100.0)
        } else {
            "N/A".into()
        };

        fn fmt_kb(kb: f64) -> String {
            if kb <= 0.0 { return "N/A".into(); }
            let gb = kb / 1024.0 / 1024.0;
            if gb >= 100.0 {
                format!("{:.0}Gi", gb)
            } else {
                format!("{:.1}Gi", gb)
            }
        }

        MemoryInfo {
            total: fmt_kb(total_kb),
            used: fmt_kb(used_kb),
            pct,
        }
    }

    /// 采集 GPU 信息
    fn collect_gpus() -> Vec<GpuInfo> {
        // 优先 nvidia-smi
        if let Ok((code, out, _)) = Self::run_command(
            "nvidia-smi --query-gpu=index,name,memory.used,memory.total,utilization.gpu --format=csv,noheader 2>/dev/null || echo 'N/A'"
        ) {
            if code == 0 && out != "N/A" {
                return out
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| {
                        let parts: Vec<&str> = l.split(',').map(|s| s.trim()).collect();
                        GpuInfo {
                            index: parts.first().unwrap_or(&"").to_string(),
                            model: parts.get(1).unwrap_or(&"").to_string(),
                            mem_used: parts.get(2).unwrap_or(&"").to_string(),
                            mem_total: parts.get(3).unwrap_or(&"").to_string(),
                            compute_util_pct: parts.get(4).unwrap_or(&"").to_string(),
                            ..Default::default()
                        }
                    })
                    .collect();
            }
        }

        vec![]
    }

    /// 采集磁盘信息
    fn collect_disks() -> Vec<DiskInfo> {
        if let Ok((code, out, _)) = Self::run_command(
            "df -h 2>/dev/null | awk 'NR>1 {printf \"%s,%s,%s,%s,%s\\n\", $6,$2,$3,$4,$5}' || echo 'N/A'"
        ) {
            if code == 0 && out != "N/A" {
                return out
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| {
                        let parts: Vec<&str> = l.split(',').collect();
                        DiskInfo {
                            mount: parts.first().unwrap_or(&"").to_string(),
                            total: parts.get(1).unwrap_or(&"").to_string(),
                            used: parts.get(2).unwrap_or(&"").to_string(),
                            avail: parts.get(3).unwrap_or(&"").to_string(),
                            usage_pct: parts.get(4).unwrap_or(&"0%").to_string(),
                        }
                    })
                    .collect();
            }
        }
        vec![]
    }

    /// 采集节点完整信息
    fn collect_node_info() -> NodeInfo {
        let hostname = Self::read_line("/proc/sys/kernel/hostname");
        let (cpu_model, cpu_cores) = Self::collect_cpu();
        let cpu_arch = Self::read_line("/proc/sys/kernel/arch");
        let memory = Self::collect_memory();
        let gpus = Self::collect_gpus();
        let disks = Self::collect_disks();

        let (uptime, _load_avg) = Self::read_line("/proc/uptime")
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|secs| {
                let days = (secs / 86400.0) as u64;
                let hours = ((secs % 86400.0) / 3600.0) as u64;
                let mins = ((secs % 3600.0) / 60.0) as u64;
                (format!("{}d {}h {}m", days, hours, mins), format!("{:.2}", secs))
            })
            .unwrap_or_else(|| ("N/A".into(), "0".into()));

        let load = Self::read_line("/proc/loadavg")
            .split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join(" ");

        NodeInfo {
            hostname,
            os: Self::read_line("/etc/os-release")
                .lines()
                .find(|l| l.starts_with("PRETTY_NAME="))
                .map(|l| l.trim_start_matches("PRETTY_NAME=").trim_matches('"').to_string())
                .unwrap_or_else(|| "N/A".into()),
            cpu_model,
            cpu_cores: cpu_cores.parse().unwrap_or(0),
            cpu_arch,
            memory: Some(memory),
            gpus,
            disks,
            uptime,
            load_avg: load,
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: Self::read_line("/proc/uptime")
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        }
    }
}

// ──────────────────────────────────────
// tonic 服务实现
// ──────────────────────────────────────

#[tonic::async_trait]
impl NodeAgent for NodeAgentService {
    async fn ping(&self, _request: Request<PingRequest>) -> Result<Response<PingResponse>, Status> {
        let state = self.state.lock().unwrap();
        let uptime = state.start_time.elapsed().as_secs() as i64;

        let load_1m = fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|s| s.split_whitespace().next()?.parse().ok())
            .unwrap_or(0.0);

        Ok(Response::new(PingResponse {
            alive: true,
            agent_version: state.version.to_string(),
            uptime_seconds: uptime,
            load_1m,
        }))
    }

    async fn get_node_info(
        &self,
        _request: Request<GetNodeInfoRequest>,
    ) -> Result<Response<NodeInfo>, Status> {
        let info = Self::collect_node_info();
        Ok(Response::new(info))
    }

    async fn exec(&self, request: Request<ExecRequest>) -> Result<Response<ExecResponse>, Status> {
        let req = request.into_inner();

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(&req.command);

        if req.timeout_secs > 0 {
            // 用 timeout 命令包装
            let wrapped = format!("timeout {} {}", req.timeout_secs, &req.command);
            cmd.arg("-c").arg(&wrapped);
        }

        let output = cmd.output().map_err(|e| Status::internal(format!("执行命令失败: {}", e)))?;

        Ok(Response::new(ExecResponse {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        }))
    }

    async fn shutdown(
        &self,
        request: Request<ShutdownRequest>,
    ) -> Result<Response<ShutdownResponse>, Status> {
        let req = request.into_inner();
        let delay = req.delay_secs.max(0);

        let cmd = if delay > 0 {
            format!(
                "shutdown -h +{} \"{}\"",
                delay,
                if req.reason.is_empty() { "Agent 发起关机" } else { &req.reason }
            )
        } else {
            format!(
                "shutdown -h now \"{}\"",
                if req.reason.is_empty() { "Agent 发起关机" } else { &req.reason }
            )
        };

        match Command::new("sh").arg("-c").arg(&cmd).output() {
            Ok(_) => Ok(Response::new(ShutdownResponse {
                scheduled: true,
                message: format!("关机指令已下发，延迟 {} 秒", delay),
            })),
            Err(e) => Ok(Response::new(ShutdownResponse {
                scheduled: false,
                message: format!("关机指令下发失败: {}", e),
            })),
        }
    }

    type WatchEventsStream = ReceiverStream<Result<NodeEvent, Status>>;

    async fn watch_events(
        &self,
        _request: Request<WatchRequest>,
    ) -> Result<Response<Self::WatchEventsStream>, Status> {
        let (tx, rx) = mpsc::channel(32);

        // 后台线程定期检查并推送事件
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;

                // 检查磁盘使用率
                let infos = NodeAgentService::collect_disks();
                for disk in &infos {
                    let pct = disk.usage_pct.trim_end_matches('%').parse::<f64>().unwrap_or(0.0);
                    if pct > 90.0 {
                        let _ = tx.send(Ok(NodeEvent {
                            event_type: "disk_high".into(),
                            severity: "critical".into(),
                            message: format!("磁盘 {} 使用率 {:.0}%", disk.mount, pct),
                            timestamp: chrono::Utc::now().timestamp(),
                            attributes: {
                                let mut m = HashMap::new();
                                m.insert("mount".into(), disk.mount.clone());
                                m.insert("usage_pct".into(), format!("{:.0}", pct));
                                m
                            },
                        })).await;
                    }
                }

                // 心跳
                let _ = tx.send(Ok(NodeEvent {
                    event_type: "heartbeat".into(),
                    severity: "info".into(),
                    message: "Agent 运行正常".into(),
                    timestamp: chrono::Utc::now().timestamp(),
                    attributes: HashMap::new(),
                })).await;
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

// ──────────────────────────────────────
// 启动入口
// ──────────────────────────────────────

pub async fn run_server(addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let service = NodeAgentService::new();

    println!(
        "┌────────────────────────────────────────────┐"
    );
    println!(
        "│  Node Agent v{}                        │",
        env!("CARGO_PKG_VERSION")
    );
    println!(
        "│  监听地址: {:<31} │",
        addr
    );
    println!(
        "│  启动时间: {}                 │",
        Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    println!(
        "└────────────────────────────────────────────┘"
    );

    Server::builder()
        .add_service(NodeAgentServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
