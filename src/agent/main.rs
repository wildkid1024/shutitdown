mod client;
mod dashboard;
mod server;

use clap::{Parser, Subcommand};
use serde::Deserialize;
use std::collections::HashMap;

// ==================== 模型部署配置 (models.toml) ====================

#[derive(Debug, Deserialize)]
struct DeployTarget {
    ip: String,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    models: Vec<DeployModel>,
}

fn default_port() -> u16 {
    50051
}

/// 模型单个实例的参数
#[derive(Debug, Clone, Deserialize)]
struct DeployInstance {
    #[serde(default)]
    port: i32,
    #[serde(default)]
    gpu_devices: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct DeployModel {
    name: String,
    #[serde(default)]
    category: String,
    /// 外部 docker-compose 文件路径 (本地，会推送到远端)
    #[serde(default)]
    compose_file: String,
    /// 实例数 (可配合 instances 表逐实例配置；缺省用此整数)
    #[serde(default)]
    instances: Vec<DeployInstance>,
}

#[derive(Debug, Deserialize)]
struct DeployConfig {
    #[serde(default)]
    targets: Vec<DeployTarget>,
}

impl DeployModel {
    /// 解析本地外部 compose 文件内容
    fn compose_yaml(&self) -> String {
        if self.compose_file.is_empty() {
            return String::new();
        }
        std::fs::read_to_string(&self.compose_file)
            .unwrap_or_else(|e| {
                eprintln!("读取外部 compose 文件 {} 失败: {}", self.compose_file, e);
                std::process::exit(1);
            })
    }

    fn to_spec(&self, index: usize, inst: &DeployInstance, compose_yaml: &str) -> client::node_agent::ModelDeploySpec {
        use client::node_agent::ModelDeploySpec;
        ModelDeploySpec {
            name: self.name.clone(),
            category: self.category.clone(),
            index: index as i32,
            port: inst.port,
            gpu_devices: inst.gpu_devices.clone(),
            compose_yaml: compose_yaml.to_string(),
            env: inst.env.clone(),
        }
    }
}


#[derive(Parser)]
#[command(name = "node-agent", version, about = "节点 Agent — gRPC 远程管理")]
struct Cli {
    /// gRPC 请求超时秒数（默认 30）
    #[arg(long, global = true, default_value = "30")]
    timeout: u64,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 启动 Agent 服务端（在被控节点上运行）
    Serve {
        /// 监听地址
        #[arg(long, default_value = "0.0.0.0")]
        host: String,
        /// 监听端口
        #[arg(long, short, default_value = "50051")]
        port: u16,
    },
    /// 扫描节点并显示状态
    Scan {
        /// 目标节点, 格式 ip:port，可多个
        #[arg(required = true, num_args = 1..)]
        targets: Vec<String>,
    },
    /// 采集节点完整信息
    Collect {
        /// 目标节点, 格式 ip:port
        #[arg(required = true, num_args = 1..)]
        targets: Vec<String>,
    },
    /// 批量关机
    Shutdown {
        /// 目标节点, 格式 ip:port
        #[arg(required = true, num_args = 1..)]
        targets: Vec<String>,
        /// 关机原因
        #[arg(long, default_value = "Agent 远程关机")]
        reason: String,
        /// 延迟秒数
        #[arg(long, short, default_value = "0")]
        delay: i32,
    },
    /// 启动 TUI 仪表盘
    Dashboard,
    /// 在节点上执行命令
    Exec {
        /// 目标节点, 格式 ip:port
        #[arg(required = true, num_args = 1..)]
        targets: Vec<String>,
        /// 要执行的命令
        #[arg(required = true)]
        command: String,
        /// 命令执行超时秒数（传递给 agent 服务端）
        #[arg(long, short, default_value = "30")]
        timeout: i32,
    },
    /// 启动 Docker 容器
    DockerStart {
        /// 目标节点, 格式 ip:port
        #[arg(required = true, num_args = 1..)]
        targets: Vec<String>,
        /// 容器 ID
        #[arg(long)]
        container_id: String,
    },
    /// 停止 Docker 容器
    DockerStop {
        /// 目标节点, 格式 ip:port
        #[arg(required = true, num_args = 1..)]
        targets: Vec<String>,
        /// 容器 ID
        #[arg(long)]
        container_id: String,
    },
    /// 启动 Docker Compose 堆栈
    ComposeUp {
        /// 目标节点, 格式 ip:port
        #[arg(required = true, num_args = 1..)]
        targets: Vec<String>,
        /// 项目名称
        #[arg(long)]
        project_name: String,
    },
    /// 停止 Docker Compose 堆栈
    ComposeDown {
        /// 目标节点, 格式 ip:port
        #[arg(required = true, num_args = 1..)]
        targets: Vec<String>,
        /// 项目名称
        #[arg(long)]
        project_name: String,
    },
    /// 根据 models.toml 部署模型服务到节点
    Deploy {
        /// 配置文件路径 (models.toml)
        #[arg(default_value = "models.toml")]
        config: String,
        /// 仅展示将执行的命令，不真正部署
        #[arg(long)]
        dry_run: bool,
    },
    /// 停止并移除 models.toml 中定义的模型服务
    Undeploy {
        /// 配置文件路径 (models.toml)
        #[arg(default_value = "models.toml")]
        config: String,
        /// 仅展示，不执行
        #[arg(long)]
        dry_run: bool,
    },
}

fn parse_targets(raw: &[String]) -> Vec<(String, u16)> {
    raw.iter()
        .map(|s| {
            if let Some((ip, port_str)) = s.split_once(':') {
                let port: u16 = port_str.parse().unwrap_or(50051);
                (ip.to_string(), port)
            } else {
                (s.clone(), 50051)
            }
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    let global_timeout = cli.timeout;

    match cli.command {
        Commands::Dashboard => {
            std::thread::spawn(|| {
                dashboard::run_dashboard();
            })
            .join()
            .unwrap();
        }
        Commands::Serve { host, port } => {
            let addr: std::net::SocketAddr = format!("{}:{}", host, port).parse()?;
            server::run_server(addr).await?;
        }

        Commands::Scan { targets } => {
            let targets = parse_targets(&targets);
            println!("扫描 {} 个节点...\n", targets.len());

            let results = client::batch_scan_with_timeout(&targets, global_timeout).await;
            for (ip, result) in results {
                match result {
                    Ok(conn) => println!(
                        "✅  {:20}  v{}  运行 {}s  负载 {:.2}",
                        ip, conn.version, conn.uptime_seconds, conn.load_1m
                    ),
                    Err(e) => println!("❌  {:20}  {}", ip, e),
                }
            }
        }

        Commands::Collect { targets } => {
            let targets = parse_targets(&targets);
            println!("采集 {} 个节点信息...\n", targets.len());

            let results = client::batch_collect_with_timeout(&targets, global_timeout).await;
            for (ip, result) in results {
                match result {
                    Ok(info) => {
                        println!("✅  {}  {}", ip, client::format_node_info(&info));
                    }
                    Err(e) => println!("❌  {}  {}", ip, e),
                }
            }
        }

        Commands::Shutdown {
            targets,
            reason,
            delay,
        } => {
            let targets = parse_targets(&targets);
            println!(
                "向 {} 个节点下发关机指令 (原因: {}, 延迟: {}s)...\n",
                targets.len(),
                reason,
                delay
            );

            let results = client::batch_shutdown_with_timeout(&targets, &reason, delay, global_timeout).await;
            for (ip, result) in results {
                match result {
                    Ok(msg) => println!("✅  {:20}  {}", ip, msg),
                    Err(e) => println!("❌  {:20}  {}", ip, e),
                }
            }
        }

        Commands::Exec {
            targets,
            command,
            timeout: cmd_timeout,
        } => {
            let targets = parse_targets(&targets);
            println!("在 {} 个节点上执行: {}\n", targets.len(), command);

            let rpc_timeout = global_timeout.max(cmd_timeout as u64);
            for (ip, port) in &targets {
                let client = client::AgentClient::with_timeout(ip, *port, rpc_timeout);
                match client.exec(&command, cmd_timeout).await {
                    Ok((code, stdout, stderr)) => {
                        println!("━━━ {} ━━━ 退出码: {}", ip, code);
                        if !stdout.is_empty() {
                            println!("{}", stdout);
                        }
                        if !stderr.is_empty() {
                            eprintln!("STDERR: {}", stderr);
                        }
                    }
                    Err(e) => println!("❌  {}  {}", ip, e),
                }
            }
        }

        Commands::DockerStart {
            targets,
            container_id,
        } => {
            let targets = parse_targets(&targets);
            println!(
                "向 {} 个节点下发 Docker 容器启动指令 (容器: {})...\n",
                targets.len(),
                container_id
            );

            let results = client::batch_docker_start_with_timeout(&targets, &container_id, global_timeout).await;
            for (ip, result) in results {
                match result {
                    Ok(msg) => println!("✅  {:20}  {}", ip, msg),
                    Err(e) => println!("❌  {:20}  {}", ip, e),
                }
            }
        }

        Commands::DockerStop {
            targets,
            container_id,
        } => {
            let targets = parse_targets(&targets);
            println!(
                "向 {} 个节点下发 Docker 容器停止指令 (容器: {})...\n",
                targets.len(),
                container_id
            );

            let results = client::batch_docker_stop_with_timeout(&targets, &container_id, global_timeout).await;
            for (ip, result) in results {
                match result {
                    Ok(msg) => println!("✅  {:20}  {}", ip, msg),
                    Err(e) => println!("❌  {:20}  {}", ip, e),
                }
            }
        }

        Commands::ComposeUp {
            targets,
            project_name,
        } => {
            let targets = parse_targets(&targets);
            println!(
                "向 {} 个节点下发 Compose 启动指令 (项目: {})...\n",
                targets.len(),
                project_name
            );

            let results = client::batch_compose_up_with_timeout(&targets, &project_name, global_timeout).await;
            for (ip, result) in results {
                match result {
                    Ok(msg) => println!("✅  {:20}  {}", ip, msg),
                    Err(e) => println!("❌  {:20}  {}", ip, e),
                }
            }
        }

        Commands::ComposeDown {
            targets,
            project_name,
        } => {
            let targets = parse_targets(&targets);
            println!(
                "向 {} 个节点下发 Compose 停止指令 (项目: {})...\n",
                targets.len(),
                project_name
            );

            let results = client::batch_compose_down_with_timeout(&targets, &project_name, global_timeout).await;
            for (ip, result) in results {
                match result {
                    Ok(msg) => println!("✅  {:20}  {}", ip, msg),
                    Err(e) => println!("❌  {:20}  {}", ip, e),
                }
            }
        }

        Commands::Deploy { config, dry_run } => {
            handle_deploy(&config, dry_run, false, global_timeout).await;
        }
        Commands::Undeploy { config, dry_run } => {
            handle_deploy(&config, dry_run, true, global_timeout).await;
        }
    }

    Ok(())
}

/// 处理模型部署/卸载 (依据 models.toml)
async fn handle_deploy(config: &str, dry_run: bool, do_undeploy: bool, global_timeout: u64) {
    let text = match std::fs::read_to_string(config) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("读取 {} 失败: {}", config, e);
            std::process::exit(1);
        }
    };
    let cfg: DeployConfig = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("解析 {} 失败: {}", config, e);
            std::process::exit(1);
        }
    };
    if cfg.targets.is_empty() {
        eprintln!("配置中无部署目标");
        std::process::exit(1);
    }
    let action = if do_undeploy { "停止/移除" } else { "部署" };
    for t in &cfg.targets {
        let disp = t.name.clone().unwrap_or_else(|| t.ip.clone());
        println!("━━━ {} ({}) {} {} 个模型 ━━━", disp, t.ip, action, t.models.len());
        for m in &t.models {
            let compose_yaml = m.compose_yaml(); // 读取外部 compose 文件内容
            // 实例列表：instances 表优先，否则按 models 数量推导
            let instances = if !m.instances.is_empty() {
                m.instances.clone()
            } else {
                // 未配置实例表 -> 单实例
                vec![DeployInstance { port: 0, gpu_devices: vec![], env: HashMap::new() }]
            };
            let client = client::AgentClient::with_timeout(&t.ip, t.port, global_timeout);
            for (idx, inst) in instances.iter().enumerate() {
                let idx = idx as i32 + 1;
                let spec = m.to_spec(idx as usize, inst, &compose_yaml);
                let port_txt = if spec.port > 0 { spec.port.to_string() } else { "动态".to_string() };
                if dry_run {
                    if do_undeploy {
                        println!("  [dry-run] docker compose -p '{}-{}' down  (模型 {})", m.name, idx, m.name);
                    } else {
                        println!("  [dry-run] 部署模型 '{}-{}' 外部compose {} 端口 {}",
                            m.name, idx, if m.compose_file.is_empty() { "(无)" } else { &m.compose_file }, port_txt);
                    }
                    continue;
                }
                if do_undeploy {
                    match client.undeploy_model(spec).await {
                        Ok((true, msg)) => println!("  ✅ {}-{}  {}", m.name, idx, msg),
                        Ok((false, msg)) => println!("  ⚠️ {}-{}  {}", m.name, idx, msg),
                        Err(e) => println!("  ❌ {}-{}  {}", m.name, idx, e),
                    }
                } else {
                    match client.deploy_model(spec).await {
                        Ok((true, msg, cid)) => println!("  ✅ {}-{}  {}  ({})", m.name, idx, msg, cid),
                        Ok((false, msg, _)) => println!("  ❌ {}-{}  {}", m.name, idx, msg),
                        Err(e) => println!("  ❌ {}-{}  {}", m.name, idx, e),
                    }
                }
            }
        }
    }
}
