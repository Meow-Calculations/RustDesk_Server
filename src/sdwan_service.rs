// =============================================================================
// SD-WAN 管理服务 - 服务端 SD-WAN 节点注册与路由协调
// =============================================================================
//
// 核心职责：
//   1. 接收来自 Edge/Core 节点的注册与心跳消息
//   2. 维护全局 SD-WAN 拓扑视图（SdwanEngine 单例）
//   3. 为远控会话请求计算最优路由路径（三级回退策略）
//   4. 周期性超时检测，自动清理离线节点
//   5. 向 relay_server 暴露链路选路 API
//
// 集成说明：
//   本模块作为 relay/rendezvous 的旁路模块运行，
//   通过 tokio::spawn 启动独立的心跳检测循环。

use hbb_common::{
    log,
    tokio::{
        sync::RwLock,
        time::{interval, Duration},
    },
};
use rawne::sdwan::{
    Isp, NodeInfo, NodeType, Region, RoutingTier, SdwanCommand, SdwanEngine, Tunnel,
};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// 全局 SD-WAN 引擎单例
// ---------------------------------------------------------------------------

lazy_static::lazy_static! {
    /// 全局 SD-WAN 引擎实例（读写锁保护，支持高并发读取、低频写入）
    static ref SDWAN_ENGINE: Arc<RwLock<SdwanEngine>> = Arc::new(RwLock::new(SdwanEngine::new()));
}

// ---------------------------------------------------------------------------
// 节点心跳检测间隔
// ---------------------------------------------------------------------------

/// 心跳超时检测周期（秒）
const HEARTBEAT_CHECK_INTERVAL_SECS: u64 = 5;

// ---------------------------------------------------------------------------
// 公开接口
// ---------------------------------------------------------------------------

/// 启动 SD-WAN 管理服务主循环
///
/// 职责：
///   1. 周期性执行节点超时检测
///   2. 消费并分发待执行的 SD-WAN 指令
///
/// 本函数应在 main.rs 启动时通过 `tokio::spawn` 调用。
pub async fn start_sdwan_service() {
    log::info!("SD-WAN 管理服务启动");
    let mut ticker = interval(Duration::from_secs(HEARTBEAT_CHECK_INTERVAL_SECS));

    loop {
        ticker.tick().await;

        let mut engine = SDWAN_ENGINE.write().await;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // 步骤一：超时检测
        engine.check_timeouts(now_ms);

        // 步骤二：消费待执行指令
        let commands = engine.drain_commands();
        for cmd in commands {
            match &cmd {
                SdwanCommand::EstablishTunnel { from_node, to_node } => {
                    log::info!(
                        "SD-WAN: 执行建立隧道指令 {} -> {}",
                        from_node, to_node
                    );
                    // TODO(RAWNE): 实际调用 QUIC/UDP 隧道建立逻辑
                }
                SdwanCommand::DestroyTunnel { tunnel_id } => {
                    log::info!("SD-WAN: 执行销毁隧道指令 {}", tunnel_id);
                    // TODO(RAWNE): 实际调用隧道销毁逻辑
                }
                SdwanCommand::EnableFecBarrage { tunnel_id } => {
                    log::info!(
                        "SD-WAN: 隧道 {} 启用 FEC 弹药倾泻模式",
                        tunnel_id
                    );
                    // TODO(RAWNE): 向隧道数据通道注入 FEC 编码参数
                }
                SdwanCommand::DisableFecBarrage { tunnel_id } => {
                    log::info!(
                        "SD-WAN: 隧道 {} 关闭 FEC 弹药倾泻模式",
                        tunnel_id
                    );
                }
                SdwanCommand::SwitchRoutingTier {
                    session_id,
                    new_tier,
                    via_tunnel,
                } => {
                    log::info!(
                        "SD-WAN: 会话 {} 切换路由层级至 {:?} (经隧道 {})",
                        session_id, new_tier, via_tunnel
                    );
                    // TODO(RAWNE): 通知 relay_server 切换该会话的数据流路由
                }
            }
        }
    }
}

/// 注册一个新的 SD-WAN 节点
///
/// # 参数
/// - `node_id`: 节点唯一 ID
/// - `node_type_str`: 节点类型字符串（"edge" 或 "core"）
/// - `isp_str`: 运营商标识（"telecom" / "unicom" / "mobile" / "bgp"）
/// - `province`: 省份
/// - `city`: 城市
/// - `public_ips`: 公网 IP 列表
/// - `bandwidth_mbps`: 总带宽容量
pub async fn register_node(
    node_id: String,
    node_type_str: &str,
    isp_str: &str,
    province: String,
    city: String,
    public_ips: Vec<String>,
    bandwidth_mbps: u32,
) {
    let node_type = match node_type_str.to_lowercase().as_str() {
        "core" => NodeType::Core,
        _ => NodeType::Edge,
    };

    let isp = match isp_str.to_lowercase().as_str() {
        "telecom" | "ct" => Isp::ChinaTelecom,
        "unicom" | "cu" => Isp::ChinaUnicom,
        "mobile" | "cm" => Isp::ChinaMobile,
        "bgp" => Isp::BgpMultiline,
        _ => Isp::Other,
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let node = NodeInfo {
        node_id,
        node_type,
        isp,
        region: Region { province, city },
        public_ips,
        bandwidth_mbps,
        is_online: true,
        last_heartbeat_ms: now_ms,
    };

    SDWAN_ENGINE.write().await.register_node(node);
}

/// 处理节点心跳
pub async fn handle_heartbeat(node_id: &str) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    SDWAN_ENGINE.write().await.on_heartbeat(node_id, now_ms);
}

/// 注销节点
pub async fn unregister_node(node_id: &str) {
    SDWAN_ENGINE.write().await.unregister_node(node_id);
}

/// 更新隧道探测数据
pub async fn update_tunnel_probe(
    tunnel_id: &str,
    rtt_us: u64,
    loss_rate: f64,
    jitter_us: u64,
) {
    SDWAN_ENGINE
        .write()
        .await
        .update_tunnel_probe(tunnel_id, rtt_us, loss_rate, jitter_us);
}

/// 添加隧道
pub async fn add_tunnel(
    tunnel_id: String,
    from_node: String,
    to_node: String,
) {
    let tunnel = Tunnel {
        tunnel_id,
        from_node,
        to_node,
        rtt_us: 0,
        loss_rate: 0.0,
        jitter_us: 0,
        fec_barrage_active: false,
        cqi_score: f64::MAX,
    };
    SDWAN_ENGINE.write().await.add_tunnel(tunnel);
}

/// 为远控会话查找最优路由路径
///
/// # 参数
/// - `client_isp_str`: 主控端运营商标识
/// - `host_isp_str`: 被控端运营商标识
///
/// # 返回值
/// - `(路由策略名称, 隧道ID)`
pub async fn find_best_route(
    client_isp_str: &str,
    host_isp_str: &str,
) -> (String, Option<String>) {
    let parse_isp = |s: &str| -> Isp {
        match s.to_lowercase().as_str() {
            "telecom" | "ct" => Isp::ChinaTelecom,
            "unicom" | "cu" => Isp::ChinaUnicom,
            "mobile" | "cm" => Isp::ChinaMobile,
            "bgp" => Isp::BgpMultiline,
            _ => Isp::Other,
        }
    };

    let engine = SDWAN_ENGINE.read().await;
    let (tier, tunnel_id) = engine.find_best_route(parse_isp(client_isp_str), parse_isp(host_isp_str));

    let tier_name = match tier {
        RoutingTier::P2pDirect => "p2p_direct".to_string(),
        RoutingTier::EdgeRelay => "edge_relay".to_string(),
        RoutingTier::BgpCoreRelay => "bgp_core_relay".to_string(),
    };

    (tier_name, tunnel_id)
}

/// 获取所有在线节点数量
pub async fn online_node_count() -> usize {
    SDWAN_ENGINE.read().await.online_nodes().len()
}

/// 获取当前所有隧道状态摘要
pub async fn tunnel_summary() -> Vec<String> {
    let engine = SDWAN_ENGINE.read().await;
    engine
        .tunnels()
        .iter()
        .map(|t| {
            format!(
                "{}: {} -> {} (RTT={}μs, Loss={:.1}%, CQI={:.1}, FEC={})",
                t.tunnel_id,
                t.from_node,
                t.to_node,
                t.rtt_us,
                t.loss_rate * 100.0,
                t.cqi_score,
                if t.fec_barrage_active { "ON" } else { "OFF" },
            )
        })
        .collect()
}
