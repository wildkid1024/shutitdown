use std::collections::HashMap;
use tonic::transport::Endpoint;

pub mod node_agent {
    tonic::include_proto!("node_agent");
}

use node_agent::node_agent_client::NodeAgentClient;
use node_agent::*;

/// 连接状态
#[derive(Debug, Clone)]
pub struct AgentConnection {
    pub ip: String,
    pub port: u16,
    pub alive: bool,
    pub version: String,
    pub uptime_seconds: i64,
    pub load_1m: f64,
}

/// 节点管理客户端
#[derive(Debug, Clone)]
pub struct AgentClient {
    endpoint: String,
}

impl AgentClient {
    pub fn new(ip: &str, port: u16) -> Self {
        Self {
            endpoint: format!("http://{}:{}", ip, port),
        }
    }

    fn client(&self) -> Result<NodeAgentClient<tonic::transport::Channel>, tonic::transport::Error> {
        let endpoint = Endpoint::from_shared(self.endpoint.clone())?
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10));
        let channel = endpoint.connect_lazy();
        Ok(NodeAgentClient::new(channel))
    }

    /// 心跳检查
    pub async fn ping(&self) -> Result<AgentConnection, String> {
        let mut client = self.client().map_err(|e| format!("连接失败: {}", e))?;
        let resp = client
            .ping(PingRequest {
                client_id: "manager".into(),
            })
            .await
            .map_err(|e| format!("Ping 失败: {}", e))?;

        let inner = resp.into_inner();
        Ok(AgentConnection {
            ip: self.endpoint.clone(),
            port: 0,
            alive: inner.alive,
            version: inner.agent_version,
            uptime_seconds: inner.uptime_seconds,
            load_1m: inner.load_1m,
        })
    }

    /// 采集节点信息
    pub async fn get_node_info(&self) -> Result<NodeInfo, String> {
        let mut client = self.client().map_err(|e| format!("连接失败: {}", e))?;
        let resp = client
            .get_node_info(GetNodeInfoRequest {})
            .await
            .map_err(|e| format!("采集失败: {}", e))?;
        Ok(resp.into_inner())
    }

    /// 执行命令
    pub async fn exec(&self, command: &str, timeout_secs: i32) -> Result<(i32, String, String), String> {
        let mut client = self.client().map_err(|e| format!("连接失败: {}", e))?;
        let resp = client
            .exec(ExecRequest {
                command: command.to_string(),
                timeout_secs,
            })
            .await
            .map_err(|e| format!("执行失败: {}", e))?;

        let inner = resp.into_inner();
        Ok((inner.exit_code, inner.stdout, inner.stderr))
    }

    /// 下发关机指令
    pub async fn shutdown(&self, reason: &str, delay_secs: i32) -> Result<String, String> {
        let mut client = self.client().map_err(|e| format!("连接失败: {}", e))?;
        let resp = client
            .shutdown(ShutdownRequest {
                reason: reason.to_string(),
                delay_secs,
            })
            .await
            .map_err(|e| format!("关机失败: {}", e))?;

        Ok(resp.into_inner().message)
    }

    /// 连接并采集完整信息（一个连接完成 ping + get_node_info）
    pub async fn connect_and_collect(&self) -> Result<AgentConnection, String> {
        self.ping().await
    }
}

/// 描述节点信息的辅助显示
pub fn format_node_info(info: &NodeInfo) -> String {
    let gpu_count = info.gpus.len();
    let disk_usage = info
        .disks
        .first()
        .map(|d| d.usage_pct.as_str())
        .unwrap_or("N/A");

    format!(
        "CPU:{} {}核 | 内存:{}/{} | GPU:{}卡 | 磁盘:{} | 负载:{} | 运行:{}",
        info.cpu_model, info.cpu_cores,
        info.memory.as_ref().map(|m| m.used.as_str()).unwrap_or("N/A"),
        info.memory.as_ref().map(|m| m.total.as_str()).unwrap_or("N/A"),
        gpu_count,
        disk_usage,
        info.load_avg,
        info.uptime,
    )
}

/// 并发批量扫描节点
pub async fn batch_scan(
    targets: &[(String, u16)],
) -> HashMap<String, Result<AgentConnection, String>> {
    use futures::future::join_all;

    let tasks: Vec<_> = targets
        .iter()
        .map(|(ip, port)| {
            let client = AgentClient::new(ip, *port);
            async move { (ip.clone(), client.ping().await) }
        })
        .collect();

    let results = join_all(tasks).await;
    let mut map = HashMap::new();
    for (ip, result) in results {
        map.insert(ip, result);
    }
    map
}

/// 并发采集多个节点的完整信息
pub async fn batch_collect(
    targets: &[(String, u16)],
) -> HashMap<String, Result<NodeInfo, String>> {
    use futures::future::join_all;

    let tasks: Vec<_> = targets
        .iter()
        .map(|(ip, port)| {
            let client = AgentClient::new(ip, *port);
            async move { (ip.clone(), client.get_node_info().await) }
        })
        .collect();

    let results = join_all(tasks).await;
    let mut map = HashMap::new();
    for (ip, result) in results {
        map.insert(ip, result);
    }
    map
}

/// 并发批量关机
pub async fn batch_shutdown(
    targets: &[(String, u16)],
    reason: &str,
    delay_secs: i32,
) -> Vec<(String, Result<String, String>)> {
    use futures::future::join_all;

    let reason = reason.to_string();
    let tasks: Vec<_> = targets
        .iter()
        .map(|(ip, port)| {
            let client = AgentClient::new(ip, *port);
            let reason = reason.clone();
            async move { (ip.clone(), client.shutdown(&reason, delay_secs).await) }
        })
        .collect();

    join_all(tasks).await
}
