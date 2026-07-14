use chrono::Local;
use dialoguer::{Confirm, MultiSelect, Password};
use ssh2::Session;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;
use serde::Deserialize;
use dirs::home_dir;

#[derive(Debug, Clone, Deserialize)]
struct Machine {
    ip: String,
    user: String,
}

#[derive(Debug, Deserialize)]
struct Config {
    machines: Vec<Machine>,
    shutdown_delay_sec: u64,
    shutdown_command: String,
    private_key_path: String,
    check_retry_times: u32,
    check_interval_ms: u64,
}

#[derive(Debug, Clone)]
enum MachineStatus {
    ShutdownComplete,
    StillOnline,
    CheckFailed,
}

impl fmt::Display for MachineStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MachineStatus::ShutdownComplete => write!(f, "✅ 已关机（端口不通）"),
            MachineStatus::StillOnline => write!(f, "❌ 仍在线（22端口可连通）"),
            MachineStatus::CheckFailed => write!(f, "⚠️ 检测异常"),
        }
    }
}

/// 写入日志文件
fn write_log(content: &str) {
    let now = Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let log_line = format!("[{}] {}\n", now, content);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open("shutdown_log.txt")
        .unwrap();
    file.write_all(log_line.as_bytes()).unwrap();
}

/// 获取私钥完整路径
fn get_private_key_path(cfg_path: &str) -> PathBuf {
    if !cfg_path.is_empty() {
        return PathBuf::from(cfg_path);
    }
    let mut path = home_dir().expect("无法获取用户家目录");
    path.push(".ssh");
    path.push("id_rsa");
    path
}

/// SSH下发关机指令
fn ssh_shutdown(m: &Machine, cmd: &str, key_path: &PathBuf, key_passphrase: &str) -> Result<(), Box<dyn std::error::Error>> {
    let addr: SocketAddr = format!("{}:22", m.ip).parse()?;
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;

    sess.userauth_pubkey_file(
        &m.user,
        None,
        key_path,
        if key_passphrase.is_empty() { None } else { Some(key_passphrase) },
    )?;

    if !sess.authenticated() {
        return Err(format!("{} 私钥认证失败", m.ip).into());
    }

    let mut channel = sess.channel_session()?;
    channel.exec(cmd)?;
    // channel.wait_close()?;

    let _ = channel.send_eof();
    let _ = channel.wait_eof();
    
    Ok(())
}

/// 单次端口检测
fn single_check(ip: &str) -> bool {
    let addr = match format!("{}:22", ip).parse::<SocketAddr>() {
        Ok(a) => a,
        Err(_) => return false,
    };
    match TcpStream::connect_timeout(&addr, Duration::from_secs(2)) {
        Ok(_) => true,
        Err(_) => false,
    }
}

/// 带重试机制的状态检测
fn check_machine_offline_with_retry(ip: &str, retry: u32, interval_ms: u64) -> MachineStatus {
    for i in 0..retry {
        let online = single_check(ip);
        if !online {
            return MachineStatus::ShutdownComplete;
        }
        println!("    [{}] 第{}次检测仍在线，等待重试...", ip, i + 1);
        std::thread::sleep(Duration::from_millis(interval_ms));
    }
    MachineStatus::StillOnline
}

fn load_config() -> Result<Config, Box<dyn std::error::Error>> {
    let text = fs::read_to_string("config.toml")?;
    let cfg: Config = toml::from_str(&text)?;
    Ok(cfg)
}

fn main() {
    write_log("===== 启动批量关机工具 =====");

    // 加载配置
    let cfg = match load_config() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("加载 config.toml 失败：{}", e);
            eprintln!("{}", msg);
            write_log(&msg);
            std::process::exit(1);
        }
    };

    if cfg.machines.is_empty() {
        let msg = "配置文件无机器列表，程序退出";
        eprintln!("{}", msg);
        write_log(msg);
        std::process::exit(1);
    }

    // 校验私钥
    let key_path = get_private_key_path(&cfg.private_key_path);
    if !key_path.exists() {
        let msg = format!("私钥文件不存在: {:?}", key_path);
        eprintln!("{}", msg);
        write_log(&msg);
        std::process::exit(1);
    }
    println!("使用SSH私钥: {:?}", key_path);
    write_log(&format!("加载私钥路径: {:?}", key_path));

    // 输入私钥密码
    let key_passphrase = Password::new()
        .with_prompt("若私钥加密，请输入密钥短语；无密码直接回车")
        .allow_empty_password(true)
        .interact()
        .unwrap();

    // 多选机器
    let items: Vec<String> = cfg.machines
        .iter()
        .map(|m| format!("{} | user:{}", m.ip, m.user))
        .collect();

    // 修复：defaults 需要bool数组，实现【默认全部勾选】
    let defaults: Vec<bool> = vec![true; items.len()];

    println!("\n===== Rust 机房批量关机工具（重试检测+日志版） =====");
    println!("空格勾选/取消，回车确认，默认全部勾选");
    let selected_idx = MultiSelect::new()
        .with_prompt("选择需要关机的机器")
        .items(&items)
        .defaults(&defaults)
        .interact()
        .unwrap();

    if selected_idx.is_empty() {
        let msg = "未选择任何机器，程序退出";
        println!("{}", msg);
        write_log(msg);
        return;
    }

    // 整理选中机器
    let selected_machines: Vec<&Machine> = selected_idx.iter().map(|&i| &cfg.machines[i]).collect();
    println!("\n即将执行关机的机器：");
    let mut select_log = String::from("选中关机机器列表：");
    for m in &selected_machines {
        println!(" - {}", m.ip);
        select_log.push_str(&format!(" {} ", m.ip));
    }
    write_log(&select_log);

    // 二次高危确认
    let confirm = Confirm::new()
        .with_prompt(format!(
            "确认下发关机指令？关机后等待{}秒，每台机器重试{}次检测在线状态",
            cfg.shutdown_delay_sec, cfg.check_retry_times
        ))
        .default(false)
        .interact()
        .unwrap();
    if !confirm {
        let msg = "用户取消本次关机操作";
        println!("{}", msg);
        write_log(msg);
        return;
    }

    // 1.下发关机指令
    println!("\n[1/3] 下发关机指令...");
    write_log("开始批量下发关机指令");
    for m in &selected_machines {
        match ssh_shutdown(m, &cfg.shutdown_command, &key_path, &key_passphrase) {
            Ok(_) => {
                let msg = format!("[{}] 关机指令下发成功", m.ip);
                println!("{}", msg);
                write_log(&msg);
            }
            Err(e) => {
                let msg = format!("[{}] 下发失败：{}", m.ip, e);
                eprintln!("{}", msg);
                write_log(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // 2.等待关机冷却
    println!("\n[2/3] 等待 {} 秒，等待服务器断电...", cfg.shutdown_delay_sec);
    write_log(&format!("等待{}秒后开始状态检测", cfg.shutdown_delay_sec));
    std::thread::sleep(Duration::from_secs(cfg.shutdown_delay_sec));

    // 3.带重试检测机器状态
    println!("\n[3/3] 检测机器在线状态（重试次数：{}）...", cfg.check_retry_times);
    write_log("开始批量检测服务器关机状态");
    let mut result_list = Vec::new();
    for m in &selected_machines {
        let status = check_machine_offline_with_retry(&m.ip, cfg.check_retry_times, cfg.check_interval_ms);
        result_list.push((m.ip.clone(), status.clone()));
        let log_msg = format!("[{}] 检测结果: {}", m.ip, status);
        write_log(&log_msg);
    }

    // 汇总输出
    println!("\n==================== 关机检测汇总 ====================");
    let mut summary_log = String::from("检测汇总结果：\n");
    for (ip, status) in result_list {
        let line = format!("{:16} | {}", ip, status);
        println!("{}", line);
        summary_log.push_str(&format!("{}\n", line));
    }
    write_log(&summary_log);
    write_log("===== 本次关机流程执行完毕 =====\n");
}