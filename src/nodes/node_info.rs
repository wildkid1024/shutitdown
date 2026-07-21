use chrono::Local;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use dialoguer::Password;
use dirs::home_dir;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Tabs},
    Frame, Terminal,
};
use serde::Deserialize;
use ssh2::Session;
use std::collections::HashSet;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

// ==================== 配置模型 ====================

#[derive(Debug, Clone, Deserialize)]
struct Machine {
    ip: String,
    user: String,
    name: Option<String>,
    tag: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Config {
    machines: Vec<Machine>,
    private_key_path: Option<String>,
    interval: u64,
}

// ==================== 单 GPU 信息 ====================

#[derive(Debug, Clone, Default)]
struct GpuInfo {
    index: String,
    model: String,
    mem_used_mb: String,
    mem_total_mb: String,
    mem_util_pct: String,
    compute_util_pct: String,
}

// ==================== 磁盘分区信息 ====================

#[derive(Debug, Clone, Default)]
struct DiskInfo {
    mount: String,
    total: String,
    used: String,
    avail: String,
    usage_pct: String,
}

// ==================== 节点信息模型 ====================

#[derive(Debug, Clone, Default)]
struct CpuDetail {
    model: String,
    cores: String,
}

#[derive(Debug, Clone, Default)]
struct MemDetail {
    total: String,
    used: String,
    pct: String,
}

#[derive(Debug, Clone)]
struct NodeInfo {
    name: String,
    tag: String,
    ip: String,
    online: bool,
    os: String,
    cpu: CpuDetail,
    cpu_arch: String,
    memory: MemDetail,
    gpus: Vec<GpuInfo>,
    disks: Vec<DiskInfo>,
    uptime: String,
    load_avg: String,
}

impl Default for NodeInfo {
    fn default() -> Self {
        NodeInfo {
            name: String::new(),
            tag: String::new(),
            ip: String::new(),
            online: false,
            os: String::new(),
            cpu: CpuDetail::default(),
            cpu_arch: String::new(),
            memory: MemDetail::default(),
            gpus: vec![],
            disks: vec![],
            uptime: String::new(),
            load_avg: String::new(),
        }
    }
}

impl NodeInfo {
    fn status_str(&self) -> &str {
        if self.online { "在线" } else { "离线" }
    }
}

// ==================== 帮助函数 ====================

fn get_private_key_path(cfg_path: &Option<String>) -> PathBuf {
    if let Some(p) = cfg_path {
        if !p.is_empty() { return PathBuf::from(p); }
    }
    let mut path = home_dir().expect("无法获取用户家目录");
    path.push(".ssh");
    path.push("id_rsa");
    path
}

fn load_config() -> Result<Config, Box<dyn std::error::Error>> {
    let text = fs::read_to_string("config.toml")?;
    let cfg: Config = toml::from_str(&text)?;
    Ok(cfg)
}

fn single_check(ip: &str) -> bool {
    let addr = match format!("{}:22", ip).parse::<SocketAddr>() {
        Ok(a) => a,
        Err(_) => return false,
    };
    TcpStream::connect_timeout(&addr, Duration::from_secs(3)).is_ok()
}

fn ssh_exec(m: &Machine, cmd: &str, key_path: &PathBuf, passphrase: &str) -> Result<String, Box<dyn std::error::Error>> {
    let addr: SocketAddr = format!("{}:22", m.ip).parse()?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;
    sess.userauth_pubkey_file(&m.user, None, key_path, if passphrase.is_empty() { None } else { Some(passphrase) })?;
    if !sess.authenticated() { return Err(format!("{} 私钥认证失败", m.ip).into()); }
    let mut channel = sess.channel_session()?;
    channel.exec(cmd)?;
    let mut output = String::new();
    channel.read_to_string(&mut output)?;
    let _ = channel.wait_close();
    Ok(output)
}

fn extract_section(output: &str, key: &str) -> String {
    let pattern = format!("==={}===", key);
    if let Some(pos) = output.find(&pattern) {
        let start = pos + pattern.len();
        let rest = &output[start..];
        let value = if let Some(next) = rest.find("===") { rest[..next].trim() } else { rest.trim() };
        value.to_string()
    } else { "N/A".to_string() }
}

fn extract_line(output: &str, key: &str) -> String {
    extract_section(output, key).lines().next().unwrap_or("N/A").trim().to_string()
}

fn parse_gpu_cards(raw: &str) -> Vec<GpuInfo> {
    let mut gpus = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line == "N/A" { continue; }
        let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if parts.len() >= 5 {
            let mem_used = parts[2].trim();
            let mem_total = parts[3].trim();
            // NPU 格式（6 字段）：index, model, mem_used, mem_total, mem_util, compute_util
            // Nvidia 格式（5 字段）：index, model, mem_used, mem_total, compute_util
            let mem_util = if parts.len() >= 6 {
                parts[4].to_string()
            } else if let (Ok(u), Ok(t)) = (mem_used.parse::<f64>(), mem_total.parse::<f64>()) {
                if t > 0.0 { format!("{:.0}", u / t * 100.0) } else { "0".into() }
            } else { "N/A".into() };
            let compute_util_idx = if parts.len() >= 6 { 5 } else { 4 };
            let fmt_mem = |v: &str| -> String {
                if let Ok(mb) = v.parse::<f64>() {
                    if mb >= 1024.0 { format!("{:.1}G", mb / 1024.0) } else { format!("{:.0}M", mb) }
                } else { v.to_string() }
            };
            gpus.push(GpuInfo {
                index: parts[0].to_string(),
                model: parts[1].trim().to_string(),
                mem_used_mb: fmt_mem(mem_used),
                mem_total_mb: fmt_mem(mem_total),
                mem_util_pct: mem_util,
                compute_util_pct: parts[compute_util_idx].to_string(),
            });
        }
    }
    if gpus.is_empty() && !raw.is_empty() && raw != "N/A" {
        for line in raw.lines() {
            let line = line.trim();
            if !line.is_empty() {
                gpus.push(GpuInfo { index: "0".into(), model: format!("其他 ({})", line), ..Default::default() });
            }
        }
    }
    gpus
}

fn parse_disks(raw: &str) -> Vec<DiskInfo> {
    let mut disks = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line == "N/A" { continue; }
        let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if parts.len() >= 5 {
            disks.push(DiskInfo {
                mount: parts[0].to_string(),
                total: parts[1].to_string(),
                used: parts[2].to_string(),
                avail: parts[3].to_string(),
                usage_pct: parts[4].to_string(),
            });
        }
    }
    disks
}

fn parse_mem_line(raw: &str) -> MemDetail {
    let raw = raw.trim();
    if raw == "N/A" || raw.is_empty() {
        return MemDetail { total: "N/A".into(), used: "N/A".into(), pct: "N/A".into() };
    }
    let parts: Vec<&str> = raw.splitn(2, ' ').collect();
    let used_total = parts[0];
    let pct = if parts.len() > 1 {
        parts[1].trim_matches('(').trim_matches(')').trim_matches('%').to_string()
    } else { "N/A".into() };
    let ut: Vec<&str> = used_total.split('/').collect();
    let used = ut.first().unwrap_or(&"N/A").to_string();
    let total = ut.get(1).unwrap_or(&"N/A").to_string();
    MemDetail { total, used, pct }
}

// ==================== 远程采集命令 ====================

fn build_collect_cmd() -> &'static str {
    r###"set -o pipefail 2>/dev/null; \
echo '===OS==='; (cat /etc/os-release 2>/dev/null | sed -n 's/PRETTY_NAME="\(.*\)"/\1/p' || echo 'N/A'); \
echo '===CPU_MODEL==='; (cat /proc/cpuinfo 2>/dev/null | grep 'model name' | head -1 | cut -d: -f2 | xargs || echo 'N/A'); \
echo '===CPU_CORES==='; nproc 2>/dev/null || echo '0'; \
echo '===CPU_ARCH==='; uname -m 2>/dev/null || echo 'N/A'; \
echo '===MEM==='; \
  mem_total=$(free | awk 'NR==2{printf "%.0f",$2}'); \
  mem_used=$(free | awk 'NR==2{printf "%.0f",$3}'); \
  if [ -n "$mem_total" ] && [ "$mem_total" -ne 0 ] 2>/dev/null; then \
    mem_pct=$((mem_used * 100 / mem_total)); \
    echo "$(free -h | awk -v p="$mem_pct" 'NR==2{printf "%s/%s (%d%%)",$3,$2,p}')"; \
  else echo 'N/A'; fi; \
echo '===LOADAVG==='; (cat /proc/loadavg 2>/dev/null | awk '{printf "%s %s %s",$1,$2,$3}' || echo 'N/A'); \
echo '===GPU_CARDS==='; \
if command -v nvidia-smi &>/dev/null; then \
  nvidia-smi --query-gpu=index,name,memory.used,memory.total,utilization.gpu --format=csv,noheader,nounits 2>/dev/null || echo 'N/A'; \
elif command -v npu-smi &>/dev/null; then \
  npu_list=$(npu-smi info -l 2>/dev/null | grep -c 'NPU'); \
  if [ "$npu_list" -gt 0 ] 2>/dev/null; then \
    for ((i=0; i<npu_list; i++)); do \
      dev_name=$(npu-smi info -t board -i $i 2>/dev/null | sed -n 's/.*Product Name[[:space:]]*:[[:space:]]*//p' | xargs); \
      usage_info=$(npu-smi info -t usages -i $i 2>/dev/null); \
      hbm_cap=$(echo "$usage_info" | grep 'HBM Capacity' | awk -F':[[:space:]]*' '{print $2}' | awk '{print $1}' | xargs); \
      hbm_pct=$(echo "$usage_info" | grep 'HBM Usage Rate' | grep -v Bandwidth | awk -F':[[:space:]]*' '{print $2}' | sed 's/%//' | xargs); \
      compute_util=$(echo "$usage_info" | grep 'Aicore Usage Rate' | awk -F':[[:space:]]*' '{print $2}' | xargs); \
      if [ -n "$hbm_cap" ] && [ -n "$hbm_pct" ]; then mem_used_mb=$((hbm_cap * hbm_pct / 100)); else mem_used_mb=0; fi; \
      echo "$i, Ascend $dev_name, ${mem_used_mb:-0}, ${hbm_cap:-0}, ${hbm_pct:-0}, ${compute_util:-0}"; \
    done; \
  else echo 'N/A'; fi; \
elif command -v rocm-smi &>/dev/null; then \
  rocm-smi --showmeminfo vram 2>/dev/null | awk -F': ' '
    /vram Total Memory \(MiB\)/ { idx=$1; gsub(/[^0-9]/,"",idx); total=$NF; gsub(/[^0-9]/,"",total) }
    /vram Total Used Memory \(MiB\)/ { used=$NF; gsub(/[^0-9]/,"",used) }
    total != "" && used != "" { printf "%s,Radeon,%s,%s,0\n", idx, used, total; total=""; used="" }' || echo 'N/A'; \
else \
  lspci 2>/dev/null | grep -iE '3D|VGA|Display' | head -5 | sed 's/.*: //' || echo 'N/A'; \
fi; \
echo '===DISKS==='; \
  df -h 2>/dev/null | awk 'NR>1 {printf "%s,%s,%s,%s,%s\n", $6,$2,$3,$4,$5}' || echo 'N/A'; \
echo '===UPTIME==='; (uptime -p 2>/dev/null || uptime 2>/dev/null || echo 'N/A')"###
}

fn collect_node_info(m: &Machine, key_path: &PathBuf, passphrase: &str) -> NodeInfo {
    let name = m.name.clone().unwrap_or_else(|| m.ip.clone());
    let tag = m.tag.clone().unwrap_or_default();
    if !single_check(&m.ip) {
        return NodeInfo { name, tag, ip: m.ip.clone(), online: false, ..Default::default() };
    }

    let cmd = build_collect_cmd();
    match ssh_exec(m, cmd, key_path, passphrase) {
        Ok(output) => {
            let cpu_model = extract_line(&output, "CPU_MODEL");
            let cpu_cores = extract_line(&output, "CPU_CORES");
            let cpu_arch = extract_line(&output, "CPU_ARCH");
            let mem_raw = extract_line(&output, "MEM");
            let load_avg = extract_line(&output, "LOADAVG");
            let gpu_raw = extract_section(&output, "GPU_CARDS");
            let disk_raw = extract_section(&output, "DISKS");
            let gpus = parse_gpu_cards(&gpu_raw);
            let disks = parse_disks(&disk_raw);
            NodeInfo {
                name, tag, ip: m.ip.clone(), online: true,
                os: extract_line(&output, "OS"),
                cpu: CpuDetail { model: cpu_model, cores: cpu_cores },
                cpu_arch,
                memory: parse_mem_line(&mem_raw),
                gpus, disks,
                uptime: extract_line(&output, "UPTIME"),
                load_avg,
            }
        }
        Err(e) => {
            let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            eprintln!("[{} {}] SSH 采集失败: {}", now, m.ip, e);
            NodeInfo { name, tag, ip: m.ip.clone(), online: false, ..Default::default() }
        }
    }
}

// ==================== TUI 应用 ====================

const TAB_NAMES: &[&str] = &["总览", "CPU", "内存", "显卡", "磁盘"];

struct App {
    nodes: Vec<NodeInfo>,
    current_tab: usize,
    scroll_offset: usize,
    selected_row: usize,       // 当前选中的行索引（在各 tab 的数据行中）
    collapsed_nodes: HashSet<String>, // 折叠的节点名称集合
    config: Config,
    last_refresh: String,      // 上次刷新时间标记
}

impl App {
    /// 计算当前 tab 的数据行总数
    fn total_rows(&self) -> usize {
        match self.current_tab {
            0 | 1 | 2 => self.nodes.len(),
            3 => {
                // GPU tab: 每个有 GPU 的节点一行摘要 + 展开时每 GPU 一行
                let mut n = 0;
                for node in &self.nodes {
                    if !node.gpus.is_empty() {
                        n += 1; // 摘要行
                        if !self.collapsed_nodes.contains(&node.name) {
                            n += node.gpus.len();
                        }
                    }
                }
                n
            }
            4 => {
                // 磁盘 tab: 每节点一行摘要 + 展开时每磁盘一行
                let mut n = 0;
                for node in &self.nodes {
                    n += 1; // 摘要行
                    if !self.collapsed_nodes.contains(&node.name) {
                        n += node.disks.len();
                    }
                }
                n
            }
            _ => 0,
        }
    }

    /// 向下移动选中行
    fn select_next(&mut self) {
        let total = self.total_rows();
        if total > 0 {
            self.selected_row = (self.selected_row + 1).min(total - 1);
            // 自动滚动：选中行超出可视区时跟随
            if self.selected_row >= self.scroll_offset + 20 {
                self.scroll_offset = self.selected_row.saturating_sub(10);
            }
        }
    }

    /// 向上移动选中行
    fn select_prev(&mut self) {
        if self.selected_row > 0 {
            self.selected_row -= 1;
            // 自动滚动：选中行移出可视区上方时跟随
            if self.selected_row < self.scroll_offset {
                self.scroll_offset = self.selected_row;
            }
        }
    }

    /// 折叠/展开当前选中行对应的节点（仅在 GPU/Disk tab 有效）
    fn toggle_selected_node(&mut self) {
        let name = match self.current_tab {
            3 => self.node_name_at_gpu_row(self.selected_row),
            4 => self.node_name_at_disk_row(self.selected_row),
            _ => None,
        };
        if let Some(name) = name {
            if self.collapsed_nodes.contains(&name) {
                self.collapsed_nodes.remove(&name);
            } else {
                self.collapsed_nodes.insert(name);
            }
        }
    }

    /// GPU tab: 根据行号找出对应的节点名
    fn node_name_at_gpu_row(&self, row: usize) -> Option<String> {
        let mut r = 0;
        for node in &self.nodes {
            if node.gpus.is_empty() {
                continue;
            }
            if r == row {
                return Some(node.name.clone());
            }
            r += 1;
            if !self.collapsed_nodes.contains(&node.name) {
                r += node.gpus.len();
                if r > row {
                    return Some(node.name.clone());
                }
            }
        }
        None
    }

    /// Disk tab: 根据行号找出对应的节点名
    fn node_name_at_disk_row(&self, row: usize) -> Option<String> {
        let mut r = 0;
        for node in &self.nodes {
            if r == row {
                return Some(node.name.clone());
            }
            r += 1;
            if !self.collapsed_nodes.contains(&node.name) {
                r += node.disks.len();
                if r > row {
                    return Some(node.name.clone());
                }
            }
        }
        None
    }
}

/// 文字进度条：████░░░░ 75%
fn text_bar(pct: f64, width: u16) -> String {
    if pct < 0.0 { return "N/A".into(); }
    let filled = ((pct / 100.0) * width as f64).round() as usize;
    let empty = (width as usize).saturating_sub(filled);
    let bar: String = std::iter::repeat('█').take(filled)
        .chain(std::iter::repeat('░').take(empty))
        .collect();
    format!("{} {:3.0}%", bar, pct)
}

/// 获取百分比数值
fn parse_pct(s: &str) -> f64 {
    s.trim_end_matches('%').trim().parse::<f64>().unwrap_or(-1.0)
}

/// 根据百分比选择颜色
fn pct_color(pct: f64) -> Color {
    if pct < 0.0 { Color::DarkGray }
    else if pct > 90.0 { Color::Red }
    else if pct > 70.0 { Color::Yellow }
    else { Color::Green }
}

// ==================== 渲染各 Tab ====================

fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let title = format!(" 节点监控仪表盘 (上次刷新: {}) ", app.last_refresh);
    let tabs = Tabs::new(TAB_NAMES.iter().map(|t| t.to_string()).collect::<Vec<_>>())
        .select(app.current_tab)
        .block(Block::default().title(title).borders(Borders::ALL))
        .style(Style::default().fg(Color::White))
        .highlight_style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Yellow))
        .divider(" │ ");
    f.render_widget(tabs, chunks[0]);

    match app.current_tab {
        0 => render_overview(f, chunks[1], app),
        1 => render_cpu(f, chunks[1], app),
        2 => render_memory(f, chunks[1], app),
        3 => render_gpu(f, chunks[1], app),
        4 => render_disk(f, chunks[1], app),
        _ => {}
    }

    let footer = Line::from(vec![
        Span::styled(" ←→切换Tab ", Style::default().fg(Color::Cyan)),
        Span::styled(" ↑↓滚动 ", Style::default().fg(Color::Cyan)),
        Span::styled(" Enter/Space折叠/展开 ", Style::default().fg(Color::Yellow)),
        Span::styled(" q退出 ", Style::default().fg(Color::Cyan)),
    ]);
    let footer_para = Paragraph::new(footer)
        .style(Style::default().fg(Color::DarkGray))
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer_para, chunks[2]);
}

/// 总览 Tab
fn render_overview(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(0)])
        .split(area);

    let total = app.nodes.len();
    let online = app.nodes.iter().filter(|n| n.online).count();
    let offline = total - online;
    let gpu_total: usize = app.nodes.iter().map(|n| n.gpus.len()).sum();
    let summary = format!(
        "总计 {} 节点 | 在线 {} | 离线 {} | GPU 总数 {}",
        total, online, offline, gpu_total
    );
    let summary_para = Paragraph::new(summary)
        .style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::ALL).title(" 概览 "));
    f.render_widget(summary_para, chunks[0]);

    let headers = vec!["名称", "标签", "状态", "IP", "OS", "CPU", "GPU", "内存", "磁盘", "运行时间"];
    let widths = [
        Constraint::Length(14), Constraint::Length(8), Constraint::Length(6),
        Constraint::Length(15), Constraint::Length(18), Constraint::Length(8),
        Constraint::Length(16), Constraint::Length(16), Constraint::Length(12), Constraint::Length(18),
    ];

    let header_cells = headers.iter().map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));
    let header_row = Row::new(header_cells).style(Style::default().bg(Color::DarkGray));

    let rows: Vec<Row> = {
        let start = app.scroll_offset;
        let visible: Vec<&NodeInfo> = app.nodes.iter().skip(start).take(20).collect();
        visible.iter().enumerate().map(|(i, n)| {
            let disk_usage = n.disks.first().map(|d| d.usage_pct.clone()).unwrap_or_else(|| "N/A".into());
            let mem_text = format!("{}/{}", n.memory.used, n.memory.total);
            let gpu_text = if n.gpus.is_empty() { "N/A".into() } else { format!("{}卡", n.gpus.len()) };
            let is_selected = start + i == app.selected_row;
            let row_style = if is_selected { Style::default().bg(Color::Blue).fg(Color::White) } else { Style::default() };
            Row::new(vec![
                Cell::from(n.name.as_str()),
                Cell::from(n.tag.as_str()),
                Cell::from(if n.online { "在线" } else { "离线" }),
                Cell::from(n.ip.as_str()),
                Cell::from(n.os.as_str()),
                Cell::from(format!("{}核", n.cpu.cores)),
                Cell::from(gpu_text),
                Cell::from(mem_text),
                Cell::from(disk_usage),
                Cell::from(n.uptime.as_str()),
            ]).style(row_style)
        }).collect()
    };

    let table = Table::new(rows, &widths)
        .header(header_row)
        .block(Block::default().borders(Borders::ALL).title(format!(" 节点列表 (共{}) ", app.nodes.len())));
    f.render_widget(table, chunks[1]);
}

/// CPU Tab
fn render_cpu(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0)])
        .split(area);

    let headers = vec!["名称", "CPU架构", "CPU型号", "核心数", "负载(1m/5m/15m)"];
    let widths = [
        Constraint::Length(14), Constraint::Length(10), Constraint::Length(48),
        Constraint::Length(8), Constraint::Length(24),
    ];

    let header_cells = headers.iter().map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));
    let header_row = Row::new(header_cells).style(Style::default().bg(Color::DarkGray));

    let rows: Vec<Row> = {
        let start = app.scroll_offset;
        let visible: Vec<&NodeInfo> = app.nodes.iter().skip(start).take(30).collect();
        visible.iter().enumerate().map(|(i, n)| {
            let is_selected = start + i == app.selected_row;
            let row_style = if is_selected { Style::default().bg(Color::Blue).fg(Color::White) } else { Style::default() };
            Row::new(vec![
                Cell::from(n.name.as_str()),
                Cell::from(n.cpu_arch.as_str()),
                Cell::from(n.cpu.model.as_str()),
                Cell::from(n.cpu.cores.as_str()),
                Cell::from(n.load_avg.as_str()),
            ]).style(row_style)
        }).collect()
    };

    let table = Table::new(rows, &widths)
        .header(header_row)
        .block(Block::default().borders(Borders::ALL).title(" CPU 信息 "));
    f.render_widget(table, chunks[0]);
}

/// 内存 Tab
fn render_memory(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0)])
        .split(area);

    let headers = vec!["名称", "已用", "总量", "内存使用率"];
    let widths = [
        Constraint::Length(14), Constraint::Length(12), Constraint::Length(12),
        Constraint::Length(40),
    ];

    let header_cells = headers.iter().map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));
    let header_row = Row::new(header_cells).style(Style::default().bg(Color::DarkGray));

    let rows: Vec<Row> = {
        let start = app.scroll_offset;
        let visible: Vec<&NodeInfo> = app.nodes.iter().skip(start).take(30).collect();
        visible.iter().enumerate().map(|(i, n)| {
            let pct = parse_pct(&n.memory.pct);
            let color = pct_color(pct);
            let bar = text_bar(pct, 20);
            let is_selected = start + i == app.selected_row;
            let row_style = if is_selected { Style::default().bg(Color::Blue).fg(Color::White) } else { Style::default() };
            Row::new(vec![
                Cell::from(n.name.as_str()),
                Cell::from(n.memory.used.as_str()),
                Cell::from(n.memory.total.as_str()),
                Cell::from(Span::styled(bar, Style::default().fg(color))),
            ]).style(row_style)
        }).collect()
    };

    let table = Table::new(rows, &widths)
        .header(header_row)
        .block(Block::default().borders(Borders::ALL).title(" 内存使用情况 "));
    f.render_widget(table, chunks[0]);
}

/// GPU Tab — 按节点折叠
fn render_gpu(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0)])
        .split(area);

    let headers = vec!["节点", "GPU#", "型号", "显存(已用/总量)", "显存占用", "算力占用"];
    let widths = [
        Constraint::Length(14), Constraint::Length(6), Constraint::Length(18),
        Constraint::Length(18), Constraint::Length(30), Constraint::Length(30),
    ];

    let header_cells = headers.iter().map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));
    let header_row = Row::new(header_cells).style(Style::default().bg(Color::DarkGray));

    let mut rows = Vec::new();
    let nodes_with_gpu: Vec<&NodeInfo> = app.nodes.iter().filter(|n| !n.gpus.is_empty()).collect();
    let mut row_idx = 0usize;
    for n in &nodes_with_gpu {
        let collapsed = app.collapsed_nodes.contains(&n.name);
        // 节点摘要行 — 显示平均内存和算力占用
        let avg_mem_pct = n.gpus.iter().map(|g| parse_pct(&g.mem_util_pct)).sum::<f64>() / n.gpus.len() as f64;
        let avg_comp_pct = n.gpus.iter().map(|g| parse_pct(&g.compute_util_pct)).sum::<f64>() / n.gpus.len() as f64;
        let summary = if collapsed {
            format!(" ▶ {}({})  显存:{:.0}%  算力:{:.0}%", n.name, n.gpus.len(), avg_mem_pct, avg_comp_pct)
        } else {
            format!(" ▼ {}({})  ", n.name, n.gpus.len())
        };
        let is_selected = row_idx == app.selected_row;
        let summary_style = if is_selected {
            Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)
        };
        rows.push(Row::new(vec![
            Cell::from(Span::styled(summary, summary_style)),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
        ]).style(if is_selected { Style::default().bg(Color::Blue) } else { Style::default() }));
        row_idx += 1;

        // 展开时显示每卡详情
        if !collapsed {
            for gpu in &n.gpus {
                let mem_pct = parse_pct(&gpu.mem_util_pct);
                let comp_pct = parse_pct(&gpu.compute_util_pct);
                let mem_bar = text_bar(mem_pct, 14);
                let comp_bar = text_bar(comp_pct, 14);
                let mem_color = pct_color(mem_pct);
                let comp_color = pct_color(comp_pct);

                let is_selected = row_idx == app.selected_row;
                let row_style = if is_selected { Style::default().bg(Color::Blue).fg(Color::White) } else { Style::default() };
                rows.push(Row::new(vec![
                    Cell::from(""),
                    Cell::from(gpu.index.as_str()),
                    Cell::from(gpu.model.as_str()),
                    Cell::from(format!("{}/{}", gpu.mem_used_mb, gpu.mem_total_mb)),
                    Cell::from(Span::styled(mem_bar, Style::default().fg(mem_color))),
                    Cell::from(Span::styled(comp_bar, Style::default().fg(comp_color))),
                ]).style(row_style));
                row_idx += 1;
            }
        }
    }

    let start = app.scroll_offset;
    let visible_rows: Vec<Row> = rows.into_iter().skip(start).take(100).collect();
    let table = Table::new(visible_rows, &widths)
        .header(header_row)
        .block(Block::default().borders(Borders::ALL).title(" GPU 信息（Enter/Space 折叠/展开节点） "));
    f.render_widget(table, chunks[0]);
}

/// 磁盘 Tab — 按节点折叠
fn render_disk(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0)])
        .split(area);

    let headers = vec!["节点", "挂载点", "总量", "已用", "可用", "使用率"];
    let widths = [
        Constraint::Length(14), Constraint::Length(14), Constraint::Length(10),
        Constraint::Length(10), Constraint::Length(10), Constraint::Length(40),
    ];

    let header_cells = headers.iter().map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));
    let header_row = Row::new(header_cells).style(Style::default().bg(Color::DarkGray));

    let mut rows = Vec::new();
    let mut row_idx = 0usize;
    for n in &app.nodes {
        let collapsed = app.collapsed_nodes.contains(&n.name);
        let summary = if collapsed {
            format!(" ▶ {}", n.name)
        } else {
            format!(" ▼ {}", n.name)
        };
        let is_selected = row_idx == app.selected_row;
        let summary_style = if is_selected {
            Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)
        };
        rows.push(Row::new(vec![
            Cell::from(Span::styled(summary, summary_style)),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
            Cell::from(""),
        ]).style(if is_selected { Style::default().bg(Color::Blue) } else { Style::default() }));
        row_idx += 1;

        if !collapsed {
            for disk in &n.disks {
                let pct = parse_pct(&disk.usage_pct);
                let color = pct_color(pct);
                let bar = text_bar(pct, 24);
                let is_selected = row_idx == app.selected_row;
                let row_style = if is_selected { Style::default().bg(Color::Blue).fg(Color::White) } else { Style::default() };
                rows.push(Row::new(vec![
                    Cell::from(""),
                    Cell::from(disk.mount.as_str()),
                    Cell::from(disk.total.as_str()),
                    Cell::from(disk.used.as_str()),
                    Cell::from(disk.avail.as_str()),
                    Cell::from(Span::styled(bar, Style::default().fg(color))),
                ]).style(row_style));
                row_idx += 1;
            }
        }
    }

    let start = app.scroll_offset;
    let visible_rows: Vec<Row> = rows.into_iter().skip(start).take(100).collect();
    let table = Table::new(visible_rows, &widths)
        .header(header_row)
        .block(Block::default().borders(Borders::ALL).title(" 磁盘使用情况（Enter/Space 折叠/展开节点） "));
    f.render_widget(table, chunks[0]);
}

// ==================== 主入口 ====================

fn main() {
    println!("\n{}", "=".repeat(70));
    println!("  机房 — 节点信息监控工具 (TUI)");
    println!("{}", "=".repeat(70));
    println!();

    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("加载 config.toml 失败：{}", e);
            std::process::exit(1);
        }
    };
    if cfg.machines.is_empty() {
        eprintln!("配置文件无机器列表，程序退出");
        std::process::exit(1);
    }

    let key_path = get_private_key_path(&cfg.private_key_path);
    if !key_path.exists() {
        eprintln!("私钥文件不存在: {:?}", key_path);
        std::process::exit(1);
    }

    let passphrase = Password::new()
        .with_prompt("若私钥加密，请输入密钥短语；无密码直接回车")
        .allow_empty_password(true)
        .interact()
        .unwrap();

    println!("发现 {} 台机器，开始采集系统信息...\n", cfg.machines.len());

    let mut nodes: Vec<NodeInfo> = Vec::with_capacity(cfg.machines.len());
    for (idx, m) in cfg.machines.iter().enumerate() {
        let name = m.name.as_deref().unwrap_or(&m.ip);
        print!("[{}/{}] {} ... ", idx + 1, cfg.machines.len(), name);
        let _ = std::io::stdout().flush();

        let info = collect_node_info(m, &key_path, &passphrase);
        if info.online {
            println!("已获取 ({} GPU, {} 磁盘分区)", info.gpus.len(), info.disks.len());
        } else {
            println!("离线/失败");
        }
        nodes.push(info);
    }

    // 写入日志
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut log_content = format!("===== 节点信息采集 [{}] =====\n", now);
    for n in &nodes {
        log_content.push_str(&format!(
            "{} | {} | {} | CPU:{} 内存:{}/{} GPU:{} 磁盘:{} | 状态:{}\n",
            n.name, n.tag, n.ip,
            n.cpu.model, n.memory.used, n.memory.total, n.gpus.len(),
            n.disks.first().map(|d| d.usage_pct.as_str()).unwrap_or("N/A"),
            n.status_str(),
        ));
    }
    log_content.push_str("================================\n\n");
    let _ = fs::write("shutdown_log.txt", log_content);

    // 自动刷新间隔
    let interval = cfg.interval;

    // 启动 TUI
    println!("\n采集完成，启动 TUI 仪表盘（自动刷新间隔 {} 秒）...", cfg.interval);
    start_tui(nodes, cfg, interval, passphrase);
}

fn start_tui(nodes: Vec<NodeInfo>, config: Config, interval: u64, passphrase: String) {
    enable_raw_mode().unwrap();
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).unwrap();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut app = App {
        nodes,
        current_tab: 0,
        scroll_offset: 0,
        selected_row: 0,
        collapsed_nodes: HashSet::new(),
        config,
        last_refresh: Local::now().format("%H:%M:%S").to_string(),
    };

    // 后台刷新线程：先采集再等待，让第一次刷新尽快到来
    let (tx, rx) = mpsc::channel();
    let machines: Vec<Machine> = app.config.machines.clone();
    let key_path = get_private_key_path(&app.config.private_key_path);
    let bg_passphrase = passphrase.clone();
    let refresh_interval = if interval == 0 { 5 } else { interval };

    thread::spawn(move || {
        loop {
            let mut fresh_nodes: Vec<NodeInfo> = Vec::with_capacity(machines.len());
            for m in &machines {
                let info = collect_node_info(m, &key_path, &bg_passphrase);
                fresh_nodes.push(info);
            }
            if tx.send(fresh_nodes).is_err() {
                break;
            }
            thread::sleep(Duration::from_secs(refresh_interval));
        }
    });

    let poll_timeout = Duration::from_millis(500);

    loop {
        terminal.draw(|f| ui(f, &mut app)).unwrap();

        // 检查是否有新数据到达（非阻塞）
        while let Ok(fresh_nodes) = rx.try_recv() {
            app.nodes = fresh_nodes;
            app.last_refresh = Local::now().format("%H:%M:%S").to_string();
        }

        if let Ok(true) = event::poll(poll_timeout) {
            if let Ok(Event::Key(key)) = event::read() {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Right | KeyCode::Tab => {
                        app.current_tab = (app.current_tab + 1) % TAB_NAMES.len();
                        app.scroll_offset = 0;
                        app.selected_row = 0;
                    }
                    KeyCode::Left => {
                        app.current_tab = if app.current_tab == 0 { TAB_NAMES.len() - 1 } else { app.current_tab - 1 };
                        app.scroll_offset = 0;
                        app.selected_row = 0;
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.select_next();
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.select_prev();
                    }
                    KeyCode::Enter | KeyCode::Char(' ') => {
                        app.toggle_selected_node();
                    }
                    _ => {}
                }
            }
        }
    }

    disable_raw_mode().unwrap();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).unwrap();
    terminal.show_cursor().unwrap();
}
