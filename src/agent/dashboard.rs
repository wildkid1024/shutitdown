use chrono::Local;
use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::Write;
use std::sync::OnceLock;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Tabs},
    Frame, Terminal,
};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crate::client::AgentClient;

// ==================== 配置模型 ====================

#[derive(Debug, Clone, Deserialize)]
struct Machine {
    ip: String,
    name: Option<String>,
    tag: Option<String>,
    /// Agent gRPC 端口，默认 50051
    agent_port: Option<u16>,
    /// 该节点 gRPC 请求超时秒数，默认 30
    agent_timeout: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Config {
    machines: Vec<Machine>,
    #[allow(dead_code)]
    interval: Option<u64>,
    /// gRPC 请求超时秒数，默认 30
    timeout_secs: Option<u64>,
}

// ==================== 从 protobuf 转换的节点信息 ====================

#[derive(Debug, Clone, Default)]
struct GpuInfo {
    index: String,
    model: String,
    mem_used: String,
    mem_total: String,
    mem_util_pct: String,
    compute_util_pct: String,
    containers: String,
}

#[derive(Debug, Clone, Default)]
struct DiskInfo {
    mount: String,
    total: String,
    used: String,
    avail: String,
    usage_pct: String,
}

#[derive(Debug, Clone, Default)]
struct ContainerInfo {
    container_id: String,
    name: String,
    image: String,
    status: String,
    ports: String,
}

#[derive(Debug, Clone, Default)]
struct StackContainerInfo {
    container_id: String,
    name: String,
    image: String,
    status: String,
    ports: String,
    service_name: String,
}

#[derive(Debug, Clone, Default)]
struct StackInfo {
    project_name: String,
    status: String,
    config_files: String,
    container_count: i32,
    containers: Vec<StackContainerInfo>,
}

#[derive(Debug, Clone, Default)]
struct CpuDetail {
    model: String,
    cores: i32,
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
    containers: Vec<ContainerInfo>,
    stacks: Vec<StackInfo>,
    uptime: String,
    load_avg: String,
    agent_version: String,
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
            containers: vec![],
            stacks: vec![],
            uptime: String::new(),
            load_avg: String::new(),
            agent_version: String::new(),
        }
    }
}

impl NodeInfo {
    fn status_str(&self) -> &str {
        if self.online { "在线" } else { "离线" }
    }
}

// ==================== gRPC 采集 ====================

/// 全局共享 tokio runtime，避免嵌套问题
fn get_runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    })
}

fn collect_via_grpc(m: &Machine) -> NodeInfo {
    let name = m.name.clone().unwrap_or_else(|| m.ip.clone());
    let tag = m.tag.clone().unwrap_or_default();
    let port = m.agent_port.unwrap_or(50051);

    let node_name = name.clone();
    let node_tag = tag.clone();
    let node_ip = m.ip.clone();

    let timeout = m.agent_timeout.unwrap_or(30);

    get_runtime().block_on(async {
        let client = AgentClient::with_timeout(&node_ip, port, timeout);

        let info = match client.get_node_info().await {
            Ok(pb) => {
                let gpus = pb
                    .gpus
                    .iter()
                    .map(|g| GpuInfo {
                        index: g.index.clone(),
                        model: g.model.clone(),
                        mem_used: g.mem_used.clone(),
                        mem_total: g.mem_total.clone(),
                        mem_util_pct: g.mem_util_pct.clone(),
                        compute_util_pct: g.compute_util_pct.clone(),
                        containers: g.containers.clone(),
                    })
                    .collect();

                let disks = pb
                    .disks
                    .iter()
                    .map(|d| DiskInfo {
                        mount: d.mount.clone(),
                        total: d.total.clone(),
                        used: d.used.clone(),
                        avail: d.avail.clone(),
                        usage_pct: d.usage_pct.clone(),
                    })
                    .collect();

                let containers = pb
                    .containers
                    .iter()
                    .map(|c| ContainerInfo {
                        container_id: c.container_id.clone(),
                        name: c.name.clone(),
                        image: c.image.clone(),
                        status: c.status.clone(),
                        ports: c.ports.clone(),
                    })
                    .collect();

                let stacks = pb
                    .stacks
                    .iter()
                    .map(|s| {
                        let containers = s.containers.iter().map(|c| StackContainerInfo {
                            container_id: c.container_id.clone(),
                            name: c.name.clone(),
                            image: c.image.clone(),
                            status: c.status.clone(),
                            ports: c.ports.clone(),
                            service_name: c.service_name.clone(),
                        }).collect();
                        StackInfo {
                            project_name: s.project_name.clone(),
                            status: s.status.clone(),
                            config_files: s.config_files.clone(),
                            container_count: s.container_count,
                            containers,
                        }
                    })
                    .collect();

                let mem = pb.memory.unwrap_or_default();

                NodeInfo {
                    name: node_name,
                    tag: node_tag,
                    ip: node_ip,
                    online: true,
                    os: pb.os,
                    cpu: CpuDetail {
                        model: pb.cpu_model,
                        cores: pb.cpu_cores,
                    },
                    cpu_arch: pb.cpu_arch,
                    memory: MemDetail {
                        total: mem.total,
                        used: mem.used,
                        pct: mem.pct,
                    },
                    gpus,
                    disks,
                    containers,
                    stacks,
                    uptime: pb.uptime,
                    load_avg: pb.load_avg,
                    agent_version: pb.agent_version,
                }
            }
            Err(_) => NodeInfo {
                name: node_name,
                tag: node_tag,
                ip: node_ip,
                online: false,
                ..Default::default()
            },
        };

        info
    })
}

// ==================== TUI 应用 ====================

const TAB_NAMES: &[&str] = &["总览", "CPU", "内存", "显卡", "磁盘", "容器", "堆栈"];

/// 日志弹窗状态
struct LogPopup {
    title: String,
    lines: Vec<String>,
    scroll_offset: usize,
}

struct App {
    nodes: Vec<NodeInfo>,
    config: Config,
    current_tab: usize,
    scroll_offset: usize,
    selected_row: usize,
    collapsed_nodes: HashSet<String>,
    /// 折叠的堆栈 key: 格式 "node名\0项目名"
    collapsed_stacks: HashSet<String>,
    /// 日志弹窗 (None 表示未打开)
    log_popup: Option<LogPopup>,
    last_refresh: String,
}

impl App {
    fn total_rows(&self) -> usize {
        match self.current_tab {
            0 | 1 | 2 => self.nodes.len(),
            3 => {
                let mut n = 0;
                for node in &self.nodes {
                    if !node.gpus.is_empty() {
                        n += 1;
                        if !self.collapsed_nodes.contains(&node.name) {
                            n += node.gpus.len();
                        }
                    }
                }
                n
            }
            4 => {
                let mut n = 0;
                for node in &self.nodes {
                    n += 1;
                    if !self.collapsed_nodes.contains(&node.name) {
                        n += node.disks.len();
                    }
                }
                n
            }
            5 => {
                let mut n = 0;
                for node in &self.nodes {
                    if node.containers.is_empty() {
                        continue;
                    }
                    n += 1;
                    if !self.collapsed_nodes.contains(&node.name) {
                        n += node.containers.len();
                    }
                }
                n
            }
            6 => {
                let mut n = 0;
                for node in &self.nodes {
                    if node.stacks.is_empty() {
                        continue;
                    }
                    n += 1; // 节点行
                    if self.collapsed_nodes.contains(&node.name) {
                        continue;
                    }
                    for s in &node.stacks {
                        n += 1; // 堆栈行
                        if !self.stack_is_collapsed(&node.name, &s.project_name) {
                            n += s.containers.len();
                        }
                    }
                }
                n
            }
            _ => 0,
        }
    }

    fn select_next(&mut self) {
        let total = self.total_rows();
        if total > 0 {
            self.selected_row = (self.selected_row + 1).min(total - 1);
            if self.selected_row >= self.scroll_offset + 20 {
                self.scroll_offset = self.selected_row.saturating_sub(10);
            }
        }
    }

    fn select_prev(&mut self) {
        if self.selected_row > 0 {
            self.selected_row -= 1;
            if self.selected_row < self.scroll_offset {
                self.scroll_offset = self.selected_row;
            }
        }
    }

    fn toggle_selected_node(&mut self) {
        // 堆栈 tab 特殊处理：优先折叠/展开堆栈，其次折叠/展开节点
        if self.current_tab == 6 {
            if let Some((ni, s)) = self.stack_at_row(self.selected_row) {
                let node = &self.nodes[ni];
                let key = Self::stack_key(&node.name, &s.project_name);
                if self.collapsed_stacks.contains(&key) {
                    self.collapsed_stacks.remove(&key);
                } else {
                    self.collapsed_stacks.insert(key);
                }
                return;
            }
        }
        let name = match self.current_tab {
            3 => self.node_name_at_gpu_row(self.selected_row),
            4 => self.node_name_at_disk_row(self.selected_row),
            5 => self.node_name_at_container_row(self.selected_row),
            6 => self.node_name_at_stack_row(self.selected_row),
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

    fn stack_key(node: &str, project: &str) -> String {
        format!("{}\u{0}{}", node, project)
    }

    fn stack_is_collapsed(&self, node: &str, project: &str) -> bool {
        self.collapsed_stacks.contains(&Self::stack_key(node, project))
    }

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

    fn node_name_at_container_row(&self, row: usize) -> Option<String> {
        let mut r = 0;
        for node in &self.nodes {
            if node.containers.is_empty() {
                continue;
            }
            if r == row {
                return Some(node.name.clone());
            }
            r += 1;
            if !self.collapsed_nodes.contains(&node.name) {
                r += node.containers.len();
                if r > row {
                    return Some(node.name.clone());
                }
            }
        }
        None
    }

    fn node_name_at_stack_row(&self, row: usize) -> Option<String> {
        let mut r = 0;
        for node in &self.nodes {
            if node.stacks.is_empty() {
                continue;
            }
            if r == row {
                return Some(node.name.clone());
            }
            r += 1;
            if !self.collapsed_nodes.contains(&node.name) {
                for s in &node.stacks {
                    r += 1;
                    if !self.stack_is_collapsed(&node.name, &s.project_name) {
                        r += s.containers.len();
                    }
                    if r > row {
                        return Some(node.name.clone());
                    }
                }
            }
        }
        None
    }

    /// 获取选中行的容器信息 (节点 index, 容器引用)
    fn container_at_row(&self, row: usize) -> Option<(usize, &ContainerInfo)> {
        let mut r = 0;
        for (ni, node) in self.nodes.iter().enumerate() {
            if node.containers.is_empty() {
                continue;
            }
            r += 1;
            if !self.collapsed_nodes.contains(&node.name) {
                for c in &node.containers {
                    if r == row {
                        return Some((ni, c));
                    }
                    r += 1;
                }
            }
        }
        None
    }

    /// 获取选中行的堆栈信息 (堆栈 tab 专用，计入容器行偏移)
    fn stack_at_row(&self, row: usize) -> Option<(usize, &StackInfo)> {
        let mut r = 0;
        for (ni, node) in self.nodes.iter().enumerate() {
            if node.stacks.is_empty() {
                continue;
            }
            r += 1;
            if !self.collapsed_nodes.contains(&node.name) {
                for s in &node.stacks {
                    if r == row {
                        return Some((ni, s));
                    }
                    r += 1;
                    if !self.stack_is_collapsed(&node.name, &s.project_name) {
                        r += s.containers.len();
                    }
                }
            }
        }
        None
    }

    /// 获取堆栈 tab 中选中行的容器 (节点 index, 容器引用)
    fn stack_container_at_row(&self, row: usize) -> Option<(usize, &StackContainerInfo)> {
        let mut r = 0;
        for (ni, node) in self.nodes.iter().enumerate() {
            if node.stacks.is_empty() {
                continue;
            }
            r += 1;
            if self.collapsed_nodes.contains(&node.name) {
                continue;
            }
            for s in &node.stacks {
                r += 1;
                if !self.stack_is_collapsed(&node.name, &s.project_name) {
                    for c in &s.containers {
                        if r == row {
                            return Some((ni, c));
                        }
                        r += 1;
                    }
                }
            }
        }
        None
    }

    /// 根据节点名和容器ID，在堆栈中查找匹配的容器（返回节点 index 与容器 ID）
    fn find_stack_container(
        &self,
        node_name: &str,
        container_id: &str,
    ) -> Option<(usize, String)> {
        self.nodes.iter().enumerate().find_map(|(ni, node)| {
            if node.name != node_name {
                return None;
            }
            node.stacks.iter().any(|s| {
                s.containers
                    .iter()
                    .any(|c| c.container_id == container_id)
            })
            .then(|| (ni, container_id.to_string()))
        })
    }
}

fn text_bar(pct: f64, width: u16) -> String {
    if pct < 0.0 { return "N/A".into(); }
    let filled = ((pct / 100.0) * width as f64).round() as usize;
    let empty = (width as usize).saturating_sub(filled);
    let bar: String = std::iter::repeat('█')
        .take(filled)
        .chain(std::iter::repeat('░').take(empty))
        .collect();
    format!("{} {:3.0}%", bar, pct)
}

fn parse_pct(s: &str) -> f64 {
    s.trim_end_matches('%').trim().parse::<f64>().unwrap_or(-1.0)
}

fn pct_color(pct: f64) -> Color {
    if pct < 0.0 { Color::DarkGray }
    else if pct > 90.0 { Color::Red }
    else if pct > 70.0 { Color::Yellow }
    else { Color::Green }
}

// ==================== 渲染 ====================

fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let title = format!(" 节点监控仪表盘 (gRPC Agent)  上次刷新: {} ", app.last_refresh);
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
            5 => render_containers(f, chunks[1], app),
            6 => render_stacks(f, chunks[1], app),
            _ => {}
        }

    let footer = Line::from(vec![
        Span::styled(" ←→切换Tab ", Style::default().fg(Color::Cyan)),
        Span::styled(" ↑↓滚动 ", Style::default().fg(Color::Cyan)),
        Span::styled(" Enter/Space折叠(节点/堆栈) ", Style::default().fg(Color::Yellow)),
        Span::styled(" 容器/堆栈Tab: s启动/t停止容器 l日志 ", Style::default().fg(Color::Green)),
        Span::styled(" 堆栈行: c启动/d停止堆栈 ", Style::default().fg(Color::Magenta)),
        Span::styled(" r刷新 ", Style::default().fg(Color::Green)),
        Span::styled(" q退出 ", Style::default().fg(Color::Cyan)),
    ]);
    f.render_widget(
        Paragraph::new(footer)
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::ALL)),
        chunks[2],
    );

    // 日志弹窗覆盖在最上层
    if let Some(popup) = &app.log_popup {
        render_log_popup(f, popup);
    }
}

/// 渲染日志弹窗
fn render_log_popup(f: &mut Frame, popup: &LogPopup) {
    let area = f.area();
    let w = (area.width * 3 / 4).min(120).max(50);
    let h = (area.height * 3 / 4).min(40).max(15);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup_area = Rect::new(x, y, w, h);

    // 半透明遮罩
    let mask = ratatui::widgets::Clear;
    f.render_widget(mask, popup_area);
    f.render_widget(
        ratatui::widgets::Block::default()
            .title(popup.title.clone())
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
        popup_area,
    );

    let inner = Rect {
        x: popup_area.x + 1,
        y: popup_area.y + 1,
        width: popup_area.width.saturating_sub(2),
        height: popup_area.height.saturating_sub(2),
    };

    let lines: Vec<Line> = popup
        .lines
        .iter()
        .skip(popup.scroll_offset)
        .take(inner.height as usize)
        .map(|l| Line::from(l.as_str()))
        .collect();

    f.render_widget(
        ratatui::widgets::Paragraph::new(lines)
            .style(Style::default().fg(Color::White)),
        inner,
    );
}

fn render_overview(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(4), Constraint::Min(0)])
        .split(area);

    let total = app.nodes.len();
    let online = app.nodes.iter().filter(|n| n.online).count();
    let offline = total - online;
    let gpu_total: usize = app.nodes.iter().map(|n| n.gpus.len()).sum();
    let container_total: usize = app.nodes.iter().map(|n| n.containers.len()).sum();
    let stack_total: usize = app.nodes.iter().map(|n| n.stacks.len()).sum();
    let summary = format!("总计 {} 节点 | 在线 {} | 离线 {} | GPU 总数 {} | 容器总数 {} | 堆栈总数 {}", total, online, offline, gpu_total, container_total, stack_total);

    f.render_widget(
        Paragraph::new(summary)
            .style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
            .block(Block::default().borders(Borders::ALL).title(" 概览 ")),
        area,
    );

    let headers = vec!["名称", "标签", "状态", "IP", "Agent版", "CPU", "GPU", "容器", "内存", "磁盘", "运行时间"];
    let widths = [
        Constraint::Length(14),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Length(15),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Length(16),
        Constraint::Length(12),
        Constraint::Length(18),
    ];

    let header_cells = headers
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));
    let header_row = Row::new(header_cells).style(Style::default().bg(Color::DarkGray));

    let rows: Vec<Row> = {
        let start = app.scroll_offset;
        let visible: Vec<&NodeInfo> = app.nodes.iter().skip(start).take(20).collect();
        visible
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let disk_usage = n.disks.iter().find(|d| d.mount == "/").or_else(|| n.disks.first()).map(|d| d.usage_pct.as_str()).unwrap_or("N/A");
                let mem_text = format!("{}/{}", n.memory.used, n.memory.total);
                let gpu_text = if n.gpus.is_empty() { "N/A".into() } else { format!("{}卡", n.gpus.len()) };
                let is_selected = start + i == app.selected_row;
                let row_style = if is_selected {
                    Style::default().bg(Color::Blue).fg(Color::White)
                } else {
                    Style::default()
                };
                Row::new(vec![
                    Cell::from(n.name.as_str()),
                    Cell::from(n.tag.as_str()),
                    Cell::from(if n.online { "在线" } else { "离线" }),
                    Cell::from(n.ip.as_str()),
                    Cell::from(n.agent_version.as_str()),
                    Cell::from(format!("{}核", n.cpu.cores)),
                    Cell::from(gpu_text),
                    Cell::from(n.containers.len().to_string()),
                    Cell::from(mem_text),
                    Cell::from(disk_usage),
                    Cell::from(n.uptime.as_str()),
                ])
                .style(row_style)
            })
            .collect()
    };

    f.render_widget(
        Table::new(rows, &widths)
            .header(header_row)
            .block(Block::default().borders(Borders::ALL).title(format!(" 节点列表 (共{}) ", app.nodes.len()))),
        chunks[1],
    );
}

fn render_cpu(f: &mut Frame, area: Rect, app: &App) {
    let headers = vec!["名称", "CPU架构", "CPU型号", "核心数", "负载(1m/5m/15m)"];
    let widths = [
        Constraint::Length(14),
        Constraint::Length(10),
        Constraint::Length(48),
        Constraint::Length(8),
        Constraint::Length(24),
    ];

    let header_cells = headers
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));
    let header_row = Row::new(header_cells).style(Style::default().bg(Color::DarkGray));

    let rows: Vec<Row> = {
        let start = app.scroll_offset;
        let visible: Vec<&NodeInfo> = app.nodes.iter().skip(start).take(30).collect();
        visible
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let is_selected = start + i == app.selected_row;
                let row_style = if is_selected {
                    Style::default().bg(Color::Blue).fg(Color::White)
                } else {
                    Style::default()
                };
                Row::new(vec![
                    Cell::from(n.name.as_str()),
                    Cell::from(n.cpu_arch.as_str()),
                    Cell::from(n.cpu.model.as_str()),
                    Cell::from(n.cpu.cores.to_string()),
                    Cell::from(n.load_avg.as_str()),
                ])
                .style(row_style)
            })
            .collect()
    };

    f.render_widget(
        Table::new(rows, &widths)
            .header(header_row)
            .block(Block::default().borders(Borders::ALL).title(" CPU 信息 ")),
        area,
    );
}

fn render_memory(f: &mut Frame, area: Rect, app: &App) {
    let headers = vec!["名称", "已用", "总量", "内存使用率"];
    let widths = [
        Constraint::Length(14),
        Constraint::Length(12),
        Constraint::Length(12),
        Constraint::Length(40),
    ];

    let header_cells = headers
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));
    let header_row = Row::new(header_cells).style(Style::default().bg(Color::DarkGray));

    let rows: Vec<Row> = {
        let start = app.scroll_offset;
        let visible: Vec<&NodeInfo> = app.nodes.iter().skip(start).take(30).collect();
        visible
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let pct = parse_pct(&n.memory.pct);
                let color = pct_color(pct);
                let bar = text_bar(pct, 20);
                let is_selected = start + i == app.selected_row;
                let row_style = if is_selected {
                    Style::default().bg(Color::Blue).fg(Color::White)
                } else {
                    Style::default()
                };
                Row::new(vec![
                    Cell::from(n.name.as_str()),
                    Cell::from(n.memory.used.as_str()),
                    Cell::from(n.memory.total.as_str()),
                    Cell::from(Span::styled(bar, Style::default().fg(color))),
                ])
                .style(row_style)
            })
            .collect()
    };

    f.render_widget(
        Table::new(rows, &widths)
            .header(header_row)
            .block(Block::default().borders(Borders::ALL).title(" 内存使用情况 ")),
        area,
    );
}

fn render_gpu(f: &mut Frame, area: Rect, app: &App) {
    let headers = vec!["节点", "GPU#", "型号", "显存(已用/总量)", "显存占用", "算力占用", "容器服务"];
    let widths = [
        Constraint::Length(14),
        Constraint::Length(6),
        Constraint::Length(18),
        Constraint::Length(18),
        Constraint::Length(24),
        Constraint::Length(24),
        Constraint::Length(20),
    ];

    let header_cells = headers
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));
    let header_row = Row::new(header_cells).style(Style::default().bg(Color::DarkGray));

    let mut rows = Vec::new();
    let nodes_with_gpu: Vec<&NodeInfo> = app.nodes.iter().filter(|n| !n.gpus.is_empty()).collect();
    let mut row_idx = 0usize;
    for n in &nodes_with_gpu {
        let collapsed = app.collapsed_nodes.contains(&n.name);
        let avg_mem_pct = n.gpus.iter().map(|g| parse_pct(&g.mem_util_pct)).sum::<f64>() / n.gpus.len() as f64;
        let avg_comp_pct = n
            .gpus
            .iter()
            .map(|g| parse_pct(&g.compute_util_pct))
            .sum::<f64>()
            / n.gpus.len() as f64;
        let summary = if collapsed {
            format!(" ▶ {}({})  显存:{:.0}%  算力:{:.0}%", n.name, n.gpus.len(), avg_mem_pct, avg_comp_pct)
        } else {
            format!(" ▼ {}({})  ", n.name, n.gpus.len())
        };
        let is_selected = row_idx == app.selected_row;
        let summary_style = if is_selected {
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)
        };
        rows.push(
            Row::new(vec![
                Cell::from(Span::styled(summary, summary_style)),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
            ])
            .style(if is_selected { Style::default().bg(Color::Blue) } else { Style::default() }),
        );
        row_idx += 1;

        if !collapsed {
            for gpu in &n.gpus {
                let mem_pct = parse_pct(&gpu.mem_util_pct);
                let comp_pct = parse_pct(&gpu.compute_util_pct);
                let mem_bar = text_bar(mem_pct, 12);
                let comp_bar = text_bar(comp_pct, 12);
                let mem_color = pct_color(mem_pct);
                let comp_color = pct_color(comp_pct);

                let is_selected = row_idx == app.selected_row;
                let row_style = if is_selected {
                    Style::default().bg(Color::Blue).fg(Color::White)
                } else {
                    Style::default()
                };
                let container_text = if gpu.containers.is_empty() { String::new() } else { gpu.containers.clone() };
                rows.push(
                    Row::new(vec![
                        Cell::from(""),
                        Cell::from(gpu.index.as_str()),
                        Cell::from(gpu.model.as_str()),
                        Cell::from(format!("{}/{}", gpu.mem_used, gpu.mem_total)),
                        Cell::from(Span::styled(mem_bar, Style::default().fg(mem_color))),
                        Cell::from(Span::styled(comp_bar, Style::default().fg(comp_color))),
                        Cell::from(container_text),
                    ])
                    .style(row_style),
                );
                row_idx += 1;
            }
        }
    }

    let start = app.scroll_offset;
    let visible_rows: Vec<Row> = rows.into_iter().skip(start).take(100).collect();
    f.render_widget(
        Table::new(visible_rows, &widths)
            .header(header_row)
            .block(Block::default().borders(Borders::ALL).title(" GPU 信息（Enter/Space 折叠/展开节点） ")),
        area,
    );
}

fn render_disk(f: &mut Frame, area: Rect, app: &App) {
    let headers = vec!["节点", "挂载点", "总量", "已用", "可用", "使用率"];
    let widths = [
        Constraint::Length(14),
        Constraint::Length(14),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(40),
    ];

    let header_cells = headers
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));
    let header_row = Row::new(header_cells).style(Style::default().bg(Color::DarkGray));

    let mut rows = Vec::new();
    let mut row_idx = 0usize;
    for n in &app.nodes {
        let collapsed = app.collapsed_nodes.contains(&n.name);
        let summary = if collapsed { format!(" ▶ {}", n.name) } else { format!(" ▼ {}", n.name) };
        let is_selected = row_idx == app.selected_row;
        let summary_style = if is_selected {
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)
        };
        rows.push(
            Row::new(vec![
                Cell::from(Span::styled(summary, summary_style)),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
            ])
            .style(if is_selected { Style::default().bg(Color::Blue) } else { Style::default() }),
        );
        row_idx += 1;

        if !collapsed {
            for disk in &n.disks {
                let pct = parse_pct(&disk.usage_pct);
                let color = pct_color(pct);
                let bar = text_bar(pct, 24);
                let is_selected = row_idx == app.selected_row;
                let row_style = if is_selected {
                    Style::default().bg(Color::Blue).fg(Color::White)
                } else {
                    Style::default()
                };
                rows.push(
                    Row::new(vec![
                        Cell::from(""),
                        Cell::from(disk.mount.as_str()),
                        Cell::from(disk.total.as_str()),
                        Cell::from(disk.used.as_str()),
                        Cell::from(disk.avail.as_str()),
                        Cell::from(Span::styled(bar, Style::default().fg(color))),
                    ])
                    .style(row_style),
                );
                row_idx += 1;
            }
        }
    }

    let start = app.scroll_offset;
    let visible_rows: Vec<Row> = rows.into_iter().skip(start).take(100).collect();
    f.render_widget(
        Table::new(visible_rows, &widths)
            .header(header_row)
            .block(Block::default().borders(Borders::ALL).title(" 磁盘使用情况（Enter/Space 折叠/展开节点） ")),
        area,
    );
}

fn render_containers(f: &mut Frame, area: Rect, app: &App) {
    let headers = vec!["节点", "容器ID", "名称", "镜像", "状态", "端口"];
    let widths = [
        Constraint::Length(14),
        Constraint::Length(14),
        Constraint::Length(20),
        Constraint::Length(30),
        Constraint::Length(10),
        Constraint::Length(30),
    ];

    let header_cells = headers
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));
    let header_row = Row::new(header_cells).style(Style::default().bg(Color::DarkGray));

    let mut rows = Vec::new();
    let mut row_idx = 0usize;
    for n in &app.nodes {
        if n.containers.is_empty() {
            continue;
        }
        let collapsed = app.collapsed_nodes.contains(&n.name);
        let count = n.containers.len();
        let summary = if collapsed {
            format!(" ▶ {} ({}容器)", n.name, count)
        } else {
            format!(" ▼ {} ({}容器)", n.name, count)
        };
        let is_selected = row_idx == app.selected_row;
        let summary_style = if is_selected {
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)
        };
        rows.push(
            Row::new(vec![
                Cell::from(Span::styled(summary, summary_style)),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
            ])
            .style(if is_selected { Style::default().bg(Color::Blue) } else { Style::default() }),
        );
        row_idx += 1;

        if !collapsed {
            for c in &n.containers {
                let is_selected = row_idx == app.selected_row;
                let row_style = if is_selected {
                    Style::default().bg(Color::Blue).fg(Color::White)
                } else {
                    Style::default()
                };
                rows.push(
                    Row::new(vec![
                        Cell::from(""),
                        Cell::from(c.container_id.as_str()),
                        Cell::from(c.name.as_str()),
                        Cell::from(c.image.as_str()),
                        Cell::from(c.status.as_str()),
                        Cell::from(c.ports.as_str()),
                    ])
                    .style(row_style),
                );
                row_idx += 1;
            }
        }
    }

    let start = app.scroll_offset;
    let visible_rows: Vec<Row> = rows.into_iter().skip(start).take(100).collect();
    f.render_widget(
        Table::new(visible_rows, &widths)
            .header(header_row)
            .block(Block::default().borders(Borders::ALL).title(" 容器信息（Enter/Space 折叠/展开节点） ")),
        area,
    );
}

fn render_stacks(f: &mut Frame, area: Rect, app: &App) {
    let headers = vec!["节点/项目", "容器ID", "服务名", "镜像", "状态", "端口"];
    let widths = [
        Constraint::Length(28),
        Constraint::Length(14),
        Constraint::Length(20),
        Constraint::Length(28),
        Constraint::Length(14),
        Constraint::Length(30),
    ];

    let header_cells = headers
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().add_modifier(Modifier::BOLD)));
    let header_row = Row::new(header_cells).style(Style::default().bg(Color::DarkGray));

    let mut rows = Vec::new();
    let mut row_idx = 0usize;
    for n in &app.nodes {
        if n.stacks.is_empty() {
            continue;
        }
        let collapsed = app.collapsed_nodes.contains(&n.name);
        let count = n.stacks.len();
        let summary = if collapsed {
            format!(" ▶ {} ({}堆栈)", n.name, count)
        } else {
            format!(" ▼ {} ({}堆栈)", n.name, count)
        };
        let is_selected = row_idx == app.selected_row;
        let summary_style = if is_selected {
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)
        };
        rows.push(
            Row::new(vec![
                Cell::from(Span::styled(summary, summary_style)),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
                Cell::from(""),
            ])
            .style(if is_selected { Style::default().bg(Color::Blue) } else { Style::default() }),
        );
        row_idx += 1;

        if !collapsed {
            for s in &n.stacks {
                // 堆栈摘要行（项目名跨第1列，状态在第5列）
                let stack_collapsed = app.stack_is_collapsed(&n.name, &s.project_name);
                let is_selected = row_idx == app.selected_row;
                let row_style = if is_selected {
                    Style::default().bg(Color::Blue).fg(Color::White)
                } else {
                    Style::default().fg(Color::Yellow)
                };
                let arrow = if stack_collapsed { "▶" } else { "▼" };
                let stack_label = format!("  {} {} [{}容器]", arrow, s.project_name, s.container_count);
                rows.push(
                    Row::new(vec![
                        Cell::from(Span::styled(stack_label, Style::default().fg(Color::Yellow))),
                        Cell::from(""),
                        Cell::from(""),
                        Cell::from(""),
                        Cell::from(s.status.as_str()),
                        Cell::from(""),
                    ])
                    .style(row_style),
                );
                row_idx += 1;

                // 每个堆栈下的容器详情行（仅当堆栈展开时）
                if !stack_collapsed {
                    for c in &s.containers {
                        let is_selected = row_idx == app.selected_row;
                        let row_style = if is_selected {
                            Style::default().bg(Color::Blue).fg(Color::White)
                        } else {
                            Style::default()
                        };
                        rows.push(
                            Row::new(vec![
                                Cell::from(""),
                                Cell::from(c.container_id.as_str()),
                                Cell::from(c.service_name.as_str()),
                                Cell::from(c.image.as_str()),
                                Cell::from(c.status.as_str()),
                                Cell::from(c.ports.as_str()),
                            ])
                            .style(row_style),
                        );
                        row_idx += 1;
                    }
                }
            }
        }
    }

    let start = app.scroll_offset;
    let visible_rows: Vec<Row> = rows.into_iter().skip(start).take(100).collect();
    f.render_widget(
        Table::new(visible_rows, &widths)
            .header(header_row)
            .block(Block::default().borders(Borders::ALL).title(" 堆栈信息（Enter/Space 折叠/展开节点，c 启动堆栈，d 停止堆栈） ")),
        area,
    );
}

// ==================== 主入口 ====================

pub fn run_dashboard() {
    println!("\n{}", "=".repeat(70));
    println!("  机房 — 节点监控仪表盘 (gRPC Agent)");
    println!("{}", "=".repeat(70));
    println!();

    let config_text = match fs::read_to_string("agent.toml") {
        Ok(t) => t,
        Err(e) => {
            eprintln!("读取 config.toml 失败：{}", e);
            std::process::exit(1);
        }
    };
    let cfg: Config = match toml::from_str(&config_text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("解析 config.toml 失败：{}", e);
            std::process::exit(1);
        }
    };

    if cfg.machines.is_empty() {
        eprintln!("配置文件中无机器列表");
        std::process::exit(1);
    }

    let refresh_interval = cfg.interval.unwrap_or(5).max(1);

    println!("发现 {} 台机器 (gRPC Agent)，开始采集...\n", cfg.machines.len());

    // 第一次采集（带日志）
    let nodes = collect_all(&cfg.machines, false);

    // 写入日志
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mut log = format!("===== 节点信息采集 (gRPC) [{}] =====\n", now);
    for n in &nodes {
        log.push_str(&format!(
            "{} | {} | {} | CPU:{} {}核 内存:{}/{} GPU:{} 磁盘:{} 容器:{} | 状态:{}\n",
            n.name,
            n.tag,
            n.ip,
            n.cpu.model,
            n.cpu.cores,
            n.memory.used,
            n.memory.total,
            n.gpus.len(),
            n.disks.first().map(|d| d.usage_pct.as_str()).unwrap_or("N/A"),
            n.containers.len(),
            n.status_str(),
        ));
    }
    log.push_str("================================\n\n");
    let _ = fs::write("shutdown_log.txt", log);

    println!("\n采集完成，启动 TUI 仪表盘（自动刷新间隔 {} 秒）...", refresh_interval);
    start_tui(nodes, cfg, refresh_interval);
}

/// silent=true 时不打印采集日志（TUI 后台刷新时使用）
fn collect_all(machines: &[Machine], silent: bool) -> Vec<NodeInfo> {
    let mut nodes = Vec::with_capacity(machines.len());
    for (idx, m) in machines.iter().enumerate() {
        let name = m.name.as_deref().unwrap_or(&m.ip);
        if !silent {
            print!("[{}/{}] {} ... ", idx + 1, machines.len(), name);
            let _ = std::io::stdout().flush();
        }

        let info = collect_via_grpc(m);
        if !silent {
            if info.online {
                println!(
                    "已获取 (CPU:{}核 GPU:{} 磁盘:{} 容器:{})",
                    info.cpu.cores,
                    info.gpus.len(),
                    info.disks.len(),
                    info.containers.len(),
                );
            } else {
                println!("离线/失败");
            }
        }
        nodes.push(info);
    }
    nodes
}

fn start_tui(nodes: Vec<NodeInfo>, config: Config, refresh_interval: u64) {
    enable_raw_mode().unwrap();
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).unwrap();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).unwrap();

    let mut app = App {
        nodes,
        config,
        current_tab: 0,
        scroll_offset: 0,
        selected_row: 0,
        collapsed_nodes: HashSet::new(),
        collapsed_stacks: HashSet::new(),
        log_popup: None,
        last_refresh: Local::now().format("%H:%M:%S").to_string(),
    };

    // 后台刷新
    let (tx, rx) = mpsc::channel();
    let machines = app.config.machines.clone();

    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(refresh_interval));
        let nodes = collect_all(&machines, true);
        if tx.send(nodes).is_err() {
            break;
        }
    });

    loop {
        terminal.draw(|f| ui(f, &mut app)).unwrap();

        while let Ok(fresh_nodes) = rx.try_recv() {
            app.nodes = fresh_nodes;
            app.last_refresh = Local::now().format("%H:%M:%S").to_string();
        }

        if let Ok(true) = event::poll(Duration::from_millis(500)) {
            if let Ok(Event::Key(key)) = event::read() {
                // 日志弹窗打开时的按键处理优先
                if app.log_popup.is_some() {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            app.log_popup = None;
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            if let Some(p) = app.log_popup.as_mut() {
                                let max = p.lines.len().saturating_sub(1);
                                p.scroll_offset = (p.scroll_offset + 1).min(max);
                            }
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            if let Some(p) = app.log_popup.as_mut() {
                                p.scroll_offset = p.scroll_offset.saturating_sub(1);
                            }
                        }
                        KeyCode::PageDown | KeyCode::Char('f') => {
                            if let Some(p) = app.log_popup.as_mut() {
                                p.scroll_offset = (p.scroll_offset + 20).min(p.lines.len().saturating_sub(1));
                            }
                        }
                        KeyCode::PageUp | KeyCode::Char('b') => {
                            if let Some(p) = app.log_popup.as_mut() {
                                p.scroll_offset = p.scroll_offset.saturating_sub(20);
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('r') => {
                        // 手动强制刷新
                        app.nodes = collect_all(&app.config.machines, true);
                        app.last_refresh = Local::now().format("%H:%M:%S").to_string();
                    }
                    KeyCode::Right | KeyCode::Tab => {
                        app.current_tab = (app.current_tab + 1) % TAB_NAMES.len();
                        app.scroll_offset = 0;
                        app.selected_row = 0;
                    }
                    KeyCode::Left => {
                        app.current_tab = if app.current_tab == 0 {
                            TAB_NAMES.len() - 1
                        } else {
                            app.current_tab - 1
                        };
                        app.scroll_offset = 0;
                        app.selected_row = 0;
                    }
                    KeyCode::Down | KeyCode::Char('j') => app.select_next(),
                    KeyCode::Up | KeyCode::Char('k') => app.select_prev(),
                    KeyCode::Enter | KeyCode::Char(' ') => app.toggle_selected_node(),
                    KeyCode::Char('s') if app.current_tab == 5 || app.current_tab == 6 => {
                        // 启动容器 (tab5 普通容器, tab6 堆栈内容器)
                        let target = if app.current_tab == 5 {
                            app.container_at_row(app.selected_row)
                                .map(|(ni, c)| (ni, c.container_id.clone()))
                        } else {
                            app.stack_container_at_row(app.selected_row)
                                .map(|(ni, c)| (ni, c.container_id.clone()))
                        };
                        if let Some((ni, cid)) = target {
                            let m = &app.config.machines[ni];
                            let port = m.agent_port.unwrap_or(50051);
                            let timeout = m.agent_timeout.unwrap_or(30);
                            let client = AgentClient::with_timeout(&m.ip, port, timeout);
                            let result = get_runtime().block_on(client.docker_start(&cid));
                            match result {
                                Ok(msg) => {
                                    app.nodes = collect_all(&app.config.machines, true);
                                    app.last_refresh = Local::now().format("%H:%M:%S").to_string();
                                    let _ = fs::write("/tmp/node_agent_docker_msg.txt", format!("✅ {}\n", msg));
                                }
                                Err(e) => {
                                    let _ = fs::write("/tmp/node_agent_docker_msg.txt", format!("❌ {}\n", e));
                                }
                            }
                        }
                    }
                    KeyCode::Char('t') if app.current_tab == 5 || app.current_tab == 6 => {
                        // 停止容器 (tab5 普通容器, tab6 堆栈内容器)
                        let target = if app.current_tab == 5 {
                            app.container_at_row(app.selected_row)
                                .map(|(ni, c)| (ni, c.container_id.clone()))
                        } else {
                            app.stack_container_at_row(app.selected_row)
                                .map(|(ni, c)| (ni, c.container_id.clone()))
                        };
                        if let Some((ni, cid)) = target {
                            let m = &app.config.machines[ni];
                            let port = m.agent_port.unwrap_or(50051);
                            let timeout = m.agent_timeout.unwrap_or(30);
                            let client = AgentClient::with_timeout(&m.ip, port, timeout);
                            let result = get_runtime().block_on(client.docker_stop(&cid));
                            match result {
                                Ok(msg) => {
                                    app.nodes = collect_all(&app.config.machines, true);
                                    app.last_refresh = Local::now().format("%H:%M:%S").to_string();
                                    let _ = fs::write("/tmp/node_agent_docker_msg.txt", format!("✅ {}\n", msg));
                                }
                                Err(e) => {
                                    let _ = fs::write("/tmp/node_agent_docker_msg.txt", format!("❌ {}\n", e));
                                }
                            }
                        }
                    }
                    KeyCode::Char('l') if app.current_tab == 5 || app.current_tab == 6 => {
                        // 查看容器日志 (tab5 普通容器, tab6 堆栈内容器)
                        let target = if app.current_tab == 5 {
                            let c = app.container_at_row(app.selected_row);
                            c.map(|(ni, c)| {
                                (ni, c.container_id.clone(), c.name.clone())
                            })
                        } else {
                            app.stack_container_at_row(app.selected_row).map(|(ni, c)| {
                                (ni, c.container_id.clone(), c.name.clone())
                            })
                        };
                        if let Some((ni, cid, cname)) = target {
                            let m = &app.config.machines[ni];
                            let port = m.agent_port.unwrap_or(50051);
                            let timeout = m.agent_timeout.unwrap_or(30);
                            let client = AgentClient::with_timeout(&m.ip, port, timeout);
                            let result = get_runtime().block_on(client.docker_logs(&cid, 200));
                            let title = format!(" 容器日志 [{}] - {} {} ",
                                m.name.as_deref().unwrap_or(&m.ip), cname, cid);
                            let lines: Vec<String> = match result {
                                Ok((true, logs, _)) => {
                                    if logs.is_empty() {
                                        vec!["(无日志输出)".to_string()]
                                    } else {
                                        logs.lines().map(|s| s.to_string()).collect()
                                    }
                                }
                                Ok((false, _, errmsg)) => vec![format!("❌ {}", errmsg)],
                                Err(e) => vec![format!("❌ {}", e)],
                            };
                            app.log_popup = Some(LogPopup {
                                title,
                                lines,
                                scroll_offset: 0,
                            });
                        }
                    }
                    KeyCode::Char('c') if app.current_tab == 6 => {
                        // 启动 Compose 堆栈
                        if let Some((ni, s)) = app.stack_at_row(app.selected_row) {
                            let m = &app.config.machines[ni];
                            let port = m.agent_port.unwrap_or(50051);
                            let timeout = m.agent_timeout.unwrap_or(30);
                            let client = AgentClient::with_timeout(&m.ip, port, timeout);
                            let result = get_runtime().block_on(client.compose_up(&s.project_name));
                            match result {
                                Ok(msg) => {
                                    app.nodes = collect_all(&app.config.machines, true);
                                    app.last_refresh = Local::now().format("%H:%M:%S").to_string();
                                    let _ = fs::write("/tmp/node_agent_docker_msg.txt", format!("✅ {}\n", msg));
                                }
                                Err(e) => {
                                    let _ = fs::write("/tmp/node_agent_docker_msg.txt", format!("❌ {}\n", e));
                                }
                            }
                        }
                    }
                    KeyCode::Char('d') if app.current_tab == 6 => {
                        // 停止 Compose 堆栈
                        if let Some((ni, s)) = app.stack_at_row(app.selected_row) {
                            let m = &app.config.machines[ni];
                            let port = m.agent_port.unwrap_or(50051);
                            let timeout = m.agent_timeout.unwrap_or(30);
                            let client = AgentClient::with_timeout(&m.ip, port, timeout);
                            let result = get_runtime().block_on(client.compose_down(&s.project_name));
                            match result {
                                Ok(msg) => {
                                    app.nodes = collect_all(&app.config.machines, true);
                                    app.last_refresh = Local::now().format("%H:%M:%S").to_string();
                                    let _ = fs::write("/tmp/node_agent_docker_msg.txt", format!("✅ {}\n", msg));
                                }
                                Err(e) => {
                                    let _ = fs::write("/tmp/node_agent_docker_msg.txt", format!("❌ {}\n", e));
                                }
                            }
                        }
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
