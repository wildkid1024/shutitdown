mod client;
mod dashboard;
mod server;

use clap::{Parser, Subcommand};

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
    }

    Ok(())
}
