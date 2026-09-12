use chrono::Local;
use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};
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

    /// DEBUG 日志（环境变量 NODE_AGENT_DEBUG=1 启用）
    fn debug_log(msg: String) {
        if std::env::var("NODE_AGENT_DEBUG").as_deref() == Ok("1") {
            eprintln!("[DEBUG {}] {}", Local::now().format("%H:%M:%S%.3f"), msg);
        }
    }

    /// 带耗时统计的 run_command（仅在 DEBUG 模式下额外记录）
    fn run_command_debug(label: &str, cmd: &str) -> Result<(i32, String, String), String> {
        let start = Instant::now();
        let result = Self::run_command(cmd);
        let elapsed = start.elapsed();
        Self::debug_log(format!("[{:6}ms] {} => {:?}", elapsed.as_millis(), label, result.as_ref().map(|(c, _, _)| c).ok()));
        result
    }

    /// 通过 curl --unix-socket 采集容器列表
    fn collect_containers() -> Vec<ContainerInfo> {
        let socket_path = "/var/run/docker.sock";
        if !std::path::Path::new(socket_path).exists() {
            Self::debug_log("collect_containers: docker.sock 不存在".into());
            return vec![];
        }

        let _t = Instant::now();

        let result = (|| -> Result<Vec<ContainerInfo>, String> {
            let output = Command::new("curl")
                .args([
                    "-s",
                    "--unix-socket", socket_path,
                    "http://localhost/containers/json?all=true",
                ])
                .output()
                .map_err(|e| format!("curl 执行失败: {}", e))?;

            if !output.status.success() {
                return Err(format!("curl 退出码: {:?}, stderr: {}", output.status.code(), String::from_utf8_lossy(&output.stderr)));
            }

            let body = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if body.is_empty() || body == "null" {
                return Ok(vec![]);
            }

            let containers: Vec<serde_json::Value> = serde_json::from_str(&body)
                .map_err(|e| format!("JSON 解析失败: {}", e))?;

            let result: Vec<ContainerInfo> = containers.iter().filter_map(|c| {
                let container_id = c.get("Id")?.as_str()?.chars().take(12).collect::<String>();
                Some(ContainerInfo {
                    container_id,
                    name: c.get("Names")?.as_array()?.first()?.as_str()?.trim_start_matches('/').to_string(),
                    image: c.get("Image")?.as_str()?.to_string(),
                    status: c.get("State")?.as_str()?.to_string(),
                    ports: Self::format_docker_ports(c.get("Ports")?.as_array()),
                })
            }).collect();

            Self::debug_log(format!("collect_containers via curl: {}ms, count={}", _t.elapsed().as_millis(), result.len()));
            Ok(result)
        })();

        match result {
            Ok(containers) => containers,
            Err(e) => {
                Self::debug_log(format!("collect_containers 失败: {}", e));
                vec![]
            }
        }
    }

    /// 采集 Docker Compose 堆栈
    fn collect_stacks() -> Vec<StackInfo> {
        let _t = Instant::now();

        let check = Self::run_command_debug("compose check",
            "docker compose version 2>/dev/null || echo 'N/A'"
        );
        let available = match check {
            Ok((code, ref out, _)) => code == 0 && out != "N/A" && !out.is_empty(),
            _ => false,
        };
        if !available {
            Self::debug_log("collect_stacks: docker compose 不可用".into());
            return vec![];
        }

        let result = Self::run_command_debug("docker compose ls",
            "docker compose ls --format json 2>/dev/null || echo '[]'"
        );

        match result {
            Ok((_code, ref stdout, _)) => {
                let body = stdout.trim();
                if body.is_empty() || body == "[]" || body == "null" {
                    return vec![];
                }

                #[derive(serde::Deserialize)]
                #[serde(rename_all = "PascalCase")]
                struct ComposeLsEntry { name: String, status: String, config_files: String }

                let entries: Vec<ComposeLsEntry> = match serde_json::from_str(body) {
                    Ok(e) => e,
                    Err(_) => {
                        // 可能是 NDJSON（每行一个 JSON 对象）
                        let mut result = Vec::new();
                        for line in body.lines() {
                            if let Ok(entry) = serde_json::from_str::<ComposeLsEntry>(line.trim()) {
                                result.push(entry);
                            }
                        }
                        result
                    }
                };

                let stacks: Vec<StackInfo> = entries.into_iter().map(|e| {
                    // 解析状态中的容器总数，处理 "running(2)" 或 "restarting(1), running(9)" 等格式
                    let container_count = e.status
                        .split(|c: char| c == '(' || c == ')' || c == ',')
                        .filter_map(|s| s.trim().parse::<i32>().ok())
                        .sum();

                    // 查询每个堆栈的容器详情 ({{json .}} 输出每行一个 JSON 对象)
                    let escaped = e.name.replace('\'', "'\\''");
                    let ps_cmd = format!("docker compose -p '{}' ps --format '{{{{json .}}}}' 2>/dev/null", escaped);
                    let containers = match Self::run_command_debug(&format!("compose ps {}", e.name), &ps_cmd) {
                        Ok((_, ref out, _)) => {
                            let body = out.trim();
                            if body.is_empty() || body == "[]" || body == "null" {
                                vec![]
                            } else {
                                // 兼容 JSON 数组 与 NDJSON（每行一个对象）
                                let json_values: Vec<serde_json::Value> = if body.starts_with('[') {
                                    serde_json::from_str::<Vec<serde_json::Value>>(body).unwrap_or_default()
                                } else {
                                    body.lines()
                                        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
                                        .collect()
                                };
                                json_values.into_iter().filter_map(|v| {
                                    let s = |k1: &str, k2: &str| v.get(k1).or_else(|| v.get(k2))
                                        .and_then(|x| x.as_str()).map(|x| x.to_string()).unwrap_or_default();
                                    let id_raw = s("ID", "Id");
                                    let cid: String = id_raw.chars().take(12).collect();
                                    let name = s("Name", "name");
                                    let image = s("Image", "image");
                                    let state = s("State", "state");
                                    let status = s("Status", "status");
                                    let state = if state.is_empty() { status.clone() } else { state };
                                    // 服务名优先用 Service 字段，没有则从容器名推导
                                    let svc_raw = s("Service", "service");
                                    let svc = if !svc_raw.is_empty() {
                                        svc_raw
                                    } else {
                                        name.rsplit_once(|c: char| c == '_' || c == '-')
                                            .map(|(_, last)| last.to_string())
                                            .unwrap_or_else(|| name.clone())
                                    };
                                    if name.is_empty() && image.is_empty() {
                                        None
                                    } else {
                                        Some(StackContainerInfo {
                                            container_id: if cid.is_empty() { String::from("N/A") } else { cid },
                                            name,
                                            image,
                                            status: state,
                                            ports: s("Ports", "ports"),
                                            service_name: svc,
                                        })
                                    }
                                }).collect()
                            }
                        }
                        Err(_) => vec![],
                    };

                    StackInfo {
                        project_name: e.name,
                        status: e.status,
                        config_files: e.config_files,
                        container_count,
                        containers,
                    }
                }).collect();

                Self::debug_log(format!("collect_stacks: {}ms, count={}", _t.elapsed().as_millis(), stacks.len()));
                stacks
            }
            Err(e) => {
                Self::debug_log(format!("collect_stacks 失败: {}", e));
                vec![]
            }
        }
    }

    /// 格式化 Docker 端口映射
    fn format_docker_ports(ports: Option<&Vec<serde_json::Value>>) -> String {
        let ports = match ports {
            Some(p) if !p.is_empty() => p,
            _ => return String::new(),
        };
        ports.iter().filter_map(|p| {
            let priv_p = p.get("PrivatePort")?.as_u64()?;
            let typ = p.get("Type")?.as_str().unwrap_or("tcp");
            if let Some(pub_p) = p.get("PublicPort") {
                let pub_p = pub_p.as_u64()?;
                Some(format!("{}:{}/{}", priv_p, pub_p, typ))
            } else {
                Some(format!("{}/{}", priv_p, typ))
            }
        }).collect::<Vec<_>>().join(", ")
    }

    /// 通过 curl --unix-socket 调用 Docker API
    fn docker_api_call(method: &str, endpoint: &str) -> Result<(i32, String, String), String> {
        let socket_path = "/var/run/docker.sock";
        if !std::path::Path::new(socket_path).exists() {
            return Err("docker.sock 不存在".into());
        }
        // curl -s -w %{http_code} -o /dev/null -X POST --unix-socket ... 'http://localhost/...'
        let method_flag = if method.is_empty() { String::new() } else { format!("-X {}", method) };
        let cmd = format!(
            "curl -s -o /dev/null -w '%{{http_code}}' {} --unix-socket {} 'http://localhost{}'",
            method_flag, socket_path, endpoint
        );
        Self::run_command(&cmd)
    }

    /// 采集节点完整信息
    fn collect_node_info() -> NodeInfo {
        let _t0 = Instant::now();

        let hostname = Self::read_line("/proc/sys/kernel/hostname");
        Self::debug_log(format!("hostname: {}", hostname));

        let _t = Instant::now();
        let (cpu_model, cpu_cores) = Self::collect_cpu();
        Self::debug_log(format!("collect_cpu: {}ms", _t.elapsed().as_millis()));

        let _t = Instant::now();
        let cpu_arch = Self::read_line("/proc/sys/kernel/arch");
        Self::debug_log(format!("cpu_arch: {}ms", _t.elapsed().as_millis()));

        let _t = Instant::now();
        let memory = Self::collect_memory();
        Self::debug_log(format!("collect_memory: {}ms", _t.elapsed().as_millis()));

        let _t = Instant::now();
        let gpus = Self::collect_gpus_debug();
        Self::debug_log(format!("collect_gpus: {}ms, count={}", _t.elapsed().as_millis(), gpus.len()));

        let _t = Instant::now();
        let containers = Self::collect_containers();
        Self::debug_log(format!("collect_containers: {}ms, count={}", _t.elapsed().as_millis(), containers.len()));

        let _t = Instant::now();
        let stacks = Self::collect_stacks();
        Self::debug_log(format!("collect_stacks: {}ms, count={}", _t.elapsed().as_millis(), stacks.len()));

        let _t = Instant::now();
        let disks = Self::collect_disks_debug();
        Self::debug_log(format!("collect_disks: {}ms, count={}", _t.elapsed().as_millis(), disks.len()));

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

        let info = NodeInfo {
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
            containers,
            disks,
            stacks,
            uptime,
            load_avg: load,
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: Self::read_line("/proc/uptime")
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        };

        Self::debug_log(format!("collect_node_info 总计: {}ms", _t0.elapsed().as_millis()));
        info
    }

    /// 带 DEBUG 的 GPU 采集
    fn collect_gpus_debug() -> Vec<GpuInfo> {
        let _t = Instant::now();

        // 1) nvidia-smi
        let result = Self::run_command_debug("nvidia-smi",
            "nvidia-smi --query-gpu=index,name,memory.used,memory.total,utilization.gpu --format=csv,noheader 2>/dev/null"
        );
        if let Ok((code, ref out, _)) = result {
            if code == 0 && !out.is_empty() {
                Self::debug_log(format!("nvidia-smi 成功 ({}ms), 输出行数={}", _t.elapsed().as_millis(), out.lines().count()));
                return out
                    .lines()
                    .filter(|l| !l.is_empty())
                    .filter_map(|l| {
                        let parts: Vec<&str> = l.split(',').map(|s| s.trim()).collect();
                        let model = parts.get(1).unwrap_or(&"").trim().to_string();
                        if model.is_empty() || model == "N/A" { return None; }
                        let parse_mem = |s: &str| -> Option<f64> {
                            let s = s.trim();
                            if let Ok(v) = s.parse::<f64>() { return Some(v); }
                            s.split_whitespace().next().and_then(|n| n.parse::<f64>().ok())
                        };
                        let mem_used = parts.get(2).unwrap_or(&"").to_string();
                        let mem_total = parts.get(3).unwrap_or(&"").to_string();
                        let mem_util = match (parse_mem(&mem_used), parse_mem(&mem_total)) {
                            (Some(u), Some(t)) if t > 0.0 => format!("{:.0}", u / t * 100.0),
                            _ => "N/A".into(),
                        };
                        Some(GpuInfo {
                            index: parts.first().unwrap_or(&"").to_string(),
                            model,
                            mem_used,
                            mem_total,
                            mem_util_pct: mem_util,
                            compute_util_pct: parts.get(4).unwrap_or(&"").to_string(),
                            ..Default::default()
                        })
                    })
                    .collect();
            }
        }
        Self::debug_log(format!("nvidia-smi 不可用 ({:?}, {}ms)", result.as_ref().map(|(c, o, _)| (c, o.len())), _t.elapsed().as_millis()));

        // 2) npu-smi（使用 bash 执行，兼容 for ((..)) 语法）
        let _t = Instant::now();
        let check = Self::run_command_debug("npu-smi check",
            "command -v npu-smi 2>/dev/null || echo 'N/A'"
        );
        if let Ok((code, ref out, _)) = check {
            if code == 0 && out != "N/A" && !out.is_empty() {
                // 改用 bash 执行（支持 seq 和 POSIX 语法）
                let script = r###"
list=$(npu-smi info -l 2>/dev/null | sed -n 's/Total Count[[:space:]]*:[[:space:]]*//p' | xargs)
[ -z "$list" ] || [ "$list" -eq 0 ] && exit 0
i=0
while [ "$i" -lt "$list" ]; do
  dev_name=$(npu-smi info -t board -i $i 2>/dev/null | sed -n 's/.*Product Name[[:space:]]*:[[:space:]]*//p' | xargs)
  usage_info=$(npu-smi info -t usages -i $i 2>/dev/null)
  hbm_cap=$(echo "$usage_info" | sed -n 's/.*HBM Capacity(MB)[[:space:]]*:[[:space:]]*//p' | awk '{print $1}')
  hbm_pct=$(echo "$usage_info" | sed -n '/HBM Usage Rate/ {/Bandwidth/!p}' | sed 's/.*:[[:space:]]*//' | sed 's/%//' | xargs)
  compute_util=$(echo "$usage_info" | grep 'Aicore Usage Rate' | awk -F':[[:space:]]*' '{print $2}' | xargs)
  if [ -n "$hbm_cap" ] && [ -n "$hbm_pct" ]; then mem_used_mb=$((hbm_cap * hbm_pct / 100)); else mem_used_mb=0; fi
  echo "$i, Ascend ${dev_name:-N/A}, ${mem_used_mb:-0}, ${hbm_cap:-0}, ${hbm_pct:-0}, ${compute_util:-0}"
  i=$((i + 1))
done
"###;
                let result = Self::run_command_debug("npu-smi query", script);
                if let Ok((code2, ref npu_out, _)) = result {
                    if code2 == 0 && !npu_out.is_empty() {
                        Self::debug_log(format!("npu-smi 成功 ({}ms), 输出行数={}", _t.elapsed().as_millis(), npu_out.lines().count()));
                        return npu_out
                            .lines()
                            .filter(|l| !l.is_empty())
                            .map(|l| {
                                let parts: Vec<&str> = l.split(',').map(|s| s.trim()).collect();
                                GpuInfo {
                                    index: parts.first().unwrap_or(&"").to_string(),
                                    model: parts.get(1).unwrap_or(&"").to_string(),
                                    mem_used: parts.get(2).unwrap_or(&"").to_string(),
                                    mem_total: parts.get(3).unwrap_or(&"").to_string(),
                                    mem_util_pct: parts.get(4).unwrap_or(&"").to_string(),
                                    compute_util_pct: parts.get(5).unwrap_or(&"").to_string(),
                                    ..Default::default()
                                }
                            })
                            .collect();
                    }
                }
                Self::debug_log(format!("npu-smi 无输出 ({:?}, {}ms)", result.as_ref().map(|(c, o, _)| (c, o.len())), _t.elapsed().as_millis()));
            }
        }

        // 3) rocm-smi（Hygon 海光 DCU）
        let result = Self::run_command_debug("rocm-smi",
            "rocm-smi --showmeminfo vram 2>/dev/null | awk -F': ' '
    /vram Total Memory \\(MiB\\)/ { idx=$1; gsub(/[^0-9]/,\"\",idx); total=$NF; gsub(/[^0-9]/,\"\",total) }
    /vram Total Used Memory \\(MiB\\)/ { used=$NF; gsub(/[^0-9]/,\"\",used) }
    total != \"\" && used != \"\" { printf \"%s,Radeon,%s,%s,0\\n\", idx, used, total; total=\"\"; used=\"\" }' 2>/dev/null"
        );
        if let Ok((code, ref out, _)) = result {
            if code == 0 && !out.is_empty() {
                let gpus: Vec<GpuInfo> = out
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(|l| {
                        let parts: Vec<&str> = l.split(',').map(|s| s.trim()).collect();
                        let mem_used = parts.get(2).unwrap_or(&"").to_string();
                        let mem_total = parts.get(3).unwrap_or(&"").to_string();
                        let parse_mem = |s: &str| -> Option<f64> {
                            let s = s.trim();
                            if let Ok(v) = s.parse::<f64>() { return Some(v); }
                            s.split_whitespace().next().and_then(|n| n.parse::<f64>().ok())
                        };
                        let mem_util = match (parse_mem(&mem_used), parse_mem(&mem_total)) {
                            (Some(u), Some(t)) if t > 0.0 => format!("{:.0}", u / t * 100.0),
                            _ => "N/A".into(),
                        };
                        GpuInfo {
                            index: parts.first().unwrap_or(&"").to_string(),
                            model: parts.get(1).unwrap_or(&"").to_string(),
                            mem_used,
                            mem_total,
                            mem_util_pct: mem_util,
                            compute_util_pct: parts.get(4).unwrap_or(&"").to_string(),
                            ..Default::default()
                        }
                    })
                    .collect();
                if !gpus.is_empty() {
                    Self::debug_log(format!("rocm-smi 成功 ({}ms), GPU数={}", _t.elapsed().as_millis(), gpus.len()));
                    return gpus;
                }
            }
        }

        // 4) lspci 兜底（仅识别型号和数量）
        let result = Self::run_command_debug("lspci",
            "lspci 2>/dev/null | grep -iE '3D|VGA|Display' | head -5 | sed 's/.*: //' || echo 'N/A'"
        );
        if let Ok((code, ref out, _)) = result {
            if code == 0 && out != "N/A" && !out.is_empty() {
                let gpus: Vec<GpuInfo> = out
                    .lines()
                    .enumerate()
                    .map(|(i, line)| GpuInfo {
                        index: i.to_string(),
                        model: format!("其他 ({})", line.trim()),
                        ..Default::default()
                    })
                    .collect();
                if !gpus.is_empty() {
                    Self::debug_log(format!("lspci 成功 ({}ms), GPU数={}", _t.elapsed().as_millis(), gpus.len()));
                    return gpus;
                }
            }
        }

        vec![]
    }

    /// 带 DEBUG 的磁盘采集
    fn collect_disks_debug() -> Vec<DiskInfo> {
        let _t = Instant::now();
        let result = Self::run_command_debug("df -h",
            "df -h 2>/dev/null | awk 'NR>1 {printf \"%s,%s,%s,%s,%s\\n\", $6,$2,$3,$4,$5}' || echo 'N/A'"
        );
        if let Ok((code, ref out, _)) = result {
            if code == 0 && out != "N/A" {
                let disks: Vec<DiskInfo> = out
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
                Self::debug_log(format!("df -h 成功 ({}ms), {} 个分区", _t.elapsed().as_millis(), disks.len()));
                return disks;
            }
        }
        Self::debug_log(format!("df -h 失败 ({}ms)", _t.elapsed().as_millis()));
        vec![]
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

    async fn docker_start(
        &self,
        request: Request<DockerContainerRequest>,
    ) -> Result<Response<DockerContainerResponse>, Status> {
        let container_id = request.into_inner().container_id;
        let endpoint = format!("/containers/{}/start", container_id);
        match Self::docker_api_call("POST", &endpoint) {
            Ok((_code, ref http_code, _)) => {
                let http = http_code.trim();
                if http == "204" || http == "304" {
                    Ok(Response::new(DockerContainerResponse {
                        success: true,
                        message: format!("容器 {} 已启动", container_id),
                    }))
                } else {
                    Ok(Response::new(DockerContainerResponse {
                        success: false,
                        message: format!("启动失败, HTTP {}", http),
                    }))
                }
            }
            Err(e) => Ok(Response::new(DockerContainerResponse {
                success: false,
                message: e,
            })),
        }
    }

    async fn docker_stop(
        &self,
        request: Request<DockerContainerRequest>,
    ) -> Result<Response<DockerContainerResponse>, Status> {
        let container_id = request.into_inner().container_id;
        let endpoint = format!("/containers/{}/stop", container_id);
        match Self::docker_api_call("POST", &endpoint) {
            Ok((_code, ref http_code, _)) => {
                let http = http_code.trim();
                if http == "204" || http == "304" {
                    Ok(Response::new(DockerContainerResponse {
                        success: true,
                        message: format!("容器 {} 已停止", container_id),
                    }))
                } else {
                    Ok(Response::new(DockerContainerResponse {
                        success: false,
                        message: format!("停止失败, HTTP {}", http),
                    }))
                }
            }
            Err(e) => Ok(Response::new(DockerContainerResponse {
                success: false,
                message: e,
            })),
        }
    }

    async fn compose_up(
        &self,
        request: Request<DockerComposeRequest>,
    ) -> Result<Response<DockerComposeResponse>, Status> {
        let project_name = request.into_inner().project_name;
        // shell 转义
        let escaped = project_name.replace('\'', "'\\''");
        let cmd = format!("docker compose -p '{}' up -d 2>&1", escaped);
        match Self::run_command(&cmd) {
            Ok((code, ref out, ref err)) => {
                let msg = if !out.is_empty() { out.clone() } else { err.clone() };
                Ok(Response::new(DockerComposeResponse {
                    success: code == 0,
                    message: if code == 0 {
                        format!("堆栈 {} 已启动", project_name)
                    } else {
                        format!("启动失败: {}", msg)
                    },
                }))
            }
            Err(e) => Ok(Response::new(DockerComposeResponse {
                success: false,
                message: e,
            })),
        }
    }

    async fn compose_down(
        &self,
        request: Request<DockerComposeRequest>,
    ) -> Result<Response<DockerComposeResponse>, Status> {
        let project_name = request.into_inner().project_name;
        let escaped = project_name.replace('\'', "'\\''");
        let cmd = format!("docker compose -p '{}' down 2>&1", escaped);
        match Self::run_command(&cmd) {
            Ok((code, ref out, ref err)) => {
                let msg = if !out.is_empty() { out.clone() } else { err.clone() };
                Ok(Response::new(DockerComposeResponse {
                    success: code == 0,
                    message: if code == 0 {
                        format!("堆栈 {} 已停止", project_name)
                    } else {
                        format!("停止失败: {}", msg)
                    },
                }))
            }
            Err(e) => Ok(Response::new(DockerComposeResponse {
                success: false,
                message: e,
            })),
        }
    }

    async fn docker_logs(
        &self,
        request: Request<DockerLogsRequest>,
    ) -> Result<Response<DockerLogsResponse>, Status> {
        let req = request.into_inner();
        let container_id = req.container_id.trim();
        if container_id.is_empty() {
            return Ok(Response::new(DockerLogsResponse {
                success: false,
                logs: String::new(),
                message: "容器 ID 为空".into(),
            }));
        }
        // shell 转义容器 ID
        let escaped = container_id.replace('\'', "'\\''");
        let tail = if req.tail > 0 { req.tail } else { 200 };
        let cmd = format!(
            "docker logs --tail {} '{}' 2>&1 2>/dev/null || docker logs '{}' 2>&1",
            tail, escaped, escaped
        );
        match Self::run_command(&cmd) {
            Ok((code, ref out, _)) => {
                if code == 0 {
                    Ok(Response::new(DockerLogsResponse {
                        success: true,
                        logs: out.clone(),
                        message: String::new(),
                    }))
                } else {
                    Ok(Response::new(DockerLogsResponse {
                        success: false,
                        logs: String::new(),
                        message: format!("获取日志失败: {}", out),
                    }))
                }
            }
            Err(e) => Ok(Response::new(DockerLogsResponse {
                success: false,
                logs: String::new(),
                message: e,
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
                let infos = NodeAgentService::collect_disks_debug();
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
