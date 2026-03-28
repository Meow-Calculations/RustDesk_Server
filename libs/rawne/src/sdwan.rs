// =============================================================================
// SD-WAN 隧道管理与多节点编排模块
// =============================================================================
//
// 核心职责：
//   1. 管理分布全国的一级节点（Edge）与 BGP 核心节点（Core）的拓扑注册。
//   2. 在节点之间建立 UDP/QUIC 长连接隧道，提供数据转发基础设施。
//   3. 与 RouteEngine 联动，驱动"直连 -> 同运营商边缘穿透 -> BGP 中继绕行"
//      的三级路由回退策略。
//   4. 管理 FEC 弹药倾泻策略：对 Edge->Core 的上行弱链路自动挂载冗余包。
//
// 兼容性说明：
//   本模块为纯逻辑/策略层，不直接操作网络 Socket。
//   实际的隧道建立与数据转发由外部传输层集成。

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// Edge 节点向 Core 节点注册心跳间隔（毫秒）
#[allow(dead_code)]
const NODE_HEARTBEAT_INTERVAL_MS: u64 = 1_000;

/// 节点超时判定阈值（毫秒）：超过此时间无心跳视为离线
const NODE_OFFLINE_TIMEOUT_MS: u64 = 10_000;

/// FEC 弹药倾泻触发的丢包率阈值
const FEC_BARRAGE_LOSS_THRESHOLD: f64 = 0.005;

/// FEC 弹药倾泻模式下的冗余比率（额外 30% 带宽换取 0 重传）
#[allow(dead_code)]
const FEC_BARRAGE_REDUNDANCY: f64 = 0.30;

/// BGP 中继回退的 CQI 分数阈值（低于此分数时从 Edge 直连回退到 BGP 中继）
const BGP_FALLBACK_SCORE_THRESHOLD: f64 = 50.0;

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

/// 网络节点类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeType {
    /// 一级边缘节点（单线/双线，如"四川电信"、"北京联通"）
    Edge,
    /// BGP 核心节点（多线 BGP 资源，跨运营商互通能力）
    Core,
}

/// ISP 运营商标识
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Isp {
    /// 中国电信
    ChinaTelecom,
    /// 中国联通
    ChinaUnicom,
    /// 中国移动
    ChinaMobile,
    /// BGP 多线（不区分运营商）
    BgpMultiline,
    /// 其他/未知
    Other,
}

/// 地域标识
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Region {
    /// 省份（如 "四川"、"北京"）
    pub province: String,
    /// 城市（如 "成都"、"北京"）
    pub city: String,
}

/// 网络节点注册信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    /// 节点唯一 ID
    pub node_id: String,
    /// 节点类型（Edge / Core）
    pub node_type: NodeType,
    /// 运营商
    pub isp: Isp,
    /// 地域信息
    pub region: Region,
    /// 节点公网 IP 列表（可能有多个 IP）
    pub public_ips: Vec<String>,
    /// 节点总带宽容量（Mbps）
    pub bandwidth_mbps: u32,
    /// 当前在线状态
    pub is_online: bool,
    /// 最后心跳时间戳（Unix 毫秒）
    pub last_heartbeat_ms: u64,
}

/// 节点间隧道实例
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tunnel {
    /// 隧道唯一 ID
    pub tunnel_id: String,
    /// 源节点 ID
    pub from_node: String,
    /// 目标节点 ID
    pub to_node: String,
    /// 当前隧道 RTT（微秒）
    pub rtt_us: u64,
    /// 当前隧道丢包率
    pub loss_rate: f64,
    /// 当前隧道抖动（微秒）
    pub jitter_us: u64,
    /// 是否启用了 FEC 弹药倾泻
    pub fec_barrage_active: bool,
    /// 隧道 CQI 综合评分（越低越好）
    pub cqi_score: f64,
}

/// SD-WAN 路由策略级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoutingTier {
    /// 第一级：P2P 直连（打洞成功）
    P2pDirect,
    /// 第二级：同运营商 Edge-to-Edge 穿透
    EdgeRelay,
    /// 第三级：跨运营商经 BGP 核心中继
    BgpCoreRelay,
}

/// SD-WAN 引擎输出的控制指令
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SdwanCommand {
    /// 建立节点间隧道
    EstablishTunnel {
        from_node: String,
        to_node: String,
    },
    /// 销毁节点间隧道
    DestroyTunnel { tunnel_id: String },
    /// 对指定隧道启用 FEC 弹药倾泻模式
    EnableFecBarrage { tunnel_id: String },
    /// 对指定隧道关闭 FEC 弹药倾泻模式
    DisableFecBarrage { tunnel_id: String },
    /// 切换远控会话的路由层级
    SwitchRoutingTier {
        session_id: String,
        new_tier: RoutingTier,
        via_tunnel: String,
    },
}

// ---------------------------------------------------------------------------
// SD-WAN 核心引擎
// ---------------------------------------------------------------------------

/// SD-WAN 隧道管理与多节点编排引擎
pub struct SdwanEngine {
    /// 已注册的网络节点
    nodes: Vec<NodeInfo>,
    /// 已建立的隧道
    tunnels: Vec<Tunnel>,
    /// 待执行指令队列
    pending_commands: Vec<SdwanCommand>,
    /// 当前远控会话使用的路由策略级别
    current_tier: RoutingTier,
}

impl SdwanEngine {
    /// 创建 SD-WAN 引擎实例
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            tunnels: Vec::new(),
            pending_commands: Vec::new(),
            current_tier: RoutingTier::P2pDirect,
        }
    }

    /// 注册一个网络节点
    pub fn register_node(&mut self, node: NodeInfo) {
        if !self.nodes.iter().any(|n| n.node_id == node.node_id) {
            log::info!(
                "SD-WAN: 注册节点 {} ({:?}, {:?}, {} {})",
                node.node_id,
                node.node_type,
                node.isp,
                node.region.province,
                node.region.city,
            );
            self.nodes.push(node);
        }
    }

    /// 注销一个网络节点
    pub fn unregister_node(&mut self, node_id: &str) {
        // 同时销毁关联的隧道
        let tunnel_ids: Vec<_> = self
            .tunnels
            .iter()
            .filter(|t| t.from_node == node_id || t.to_node == node_id)
            .map(|t| t.tunnel_id.clone())
            .collect();

        for tid in &tunnel_ids {
            self.pending_commands.push(SdwanCommand::DestroyTunnel {
                tunnel_id: tid.clone(),
            });
        }
        self.tunnels.retain(|t| t.from_node != node_id && t.to_node != node_id);
        self.nodes.retain(|n| n.node_id != node_id);
    }

    /// 更新节点心跳（保活检测）
    pub fn on_heartbeat(&mut self, node_id: &str, timestamp_ms: u64) {
        if let Some(node) = self.nodes.iter_mut().find(|n| n.node_id == node_id) {
            node.last_heartbeat_ms = timestamp_ms;
            node.is_online = true;
        }
    }

    /// 超时检测：将长时间无心跳的节点标记为离线
    pub fn check_timeouts(&mut self, current_time_ms: u64) {
        for node in &mut self.nodes {
            if node.is_online
                && current_time_ms.saturating_sub(node.last_heartbeat_ms) > NODE_OFFLINE_TIMEOUT_MS
            {
                log::warn!("SD-WAN: 节点 {} 超时离线", node.node_id);
                node.is_online = false;
            }
        }
    }

    /// 更新隧道探测数据
    pub fn update_tunnel_probe(
        &mut self,
        tunnel_id: &str,
        rtt_us: u64,
        loss_rate: f64,
        jitter_us: u64,
    ) {
        if let Some(tunnel) = self.tunnels.iter_mut().find(|t| t.tunnel_id == tunnel_id) {
            tunnel.rtt_us = rtt_us;
            tunnel.loss_rate = loss_rate;
            tunnel.jitter_us = jitter_us;

            // 重新计算 CQI 评分
            let rtt_ms = rtt_us as f64 / 1000.0;
            let jitter_ms = jitter_us as f64 / 1000.0;
            tunnel.cqi_score = 0.4 * rtt_ms + 0.3 * jitter_ms + 0.3 * (loss_rate * 1000.0);

            // 自动 FEC 弹药倾泻策略
            if loss_rate > FEC_BARRAGE_LOSS_THRESHOLD && !tunnel.fec_barrage_active {
                tunnel.fec_barrage_active = true;
                self.pending_commands.push(SdwanCommand::EnableFecBarrage {
                    tunnel_id: tunnel_id.to_string(),
                });
                log::info!(
                    "SD-WAN: 隧道 {} 丢包率 {:.1}% 超过阈值, 启用 FEC 弹药倾泻",
                    tunnel_id,
                    loss_rate * 100.0,
                );
            } else if loss_rate <= FEC_BARRAGE_LOSS_THRESHOLD / 2.0 && tunnel.fec_barrage_active {
                // 丢包率回落到阈值一半以下时关闭 FEC（迟滞策略防震荡）
                tunnel.fec_barrage_active = false;
                self.pending_commands
                    .push(SdwanCommand::DisableFecBarrage {
                        tunnel_id: tunnel_id.to_string(),
                    });
            }
        }
    }

    /// 为指定远控会话查找最优路由路径
    ///
    /// # 参数
    /// - `session_id`: 远控会话 ID
    /// - `client_isp`: 主控端所在运营商（通过 IP 归属库判定）
    /// - `host_isp`: 被控端所在运营商
    ///
    /// # 返回值
    /// - 返回推荐的路由策略级别和隧道 ID
    pub fn find_best_route(
        &self,
        client_isp: Isp,
        host_isp: Isp,
    ) -> (RoutingTier, Option<String>) {
        // 策略一：同运营商可以尝试 Edge 直连穿透
        if client_isp == host_isp && client_isp != Isp::Other {
            // 查找同 ISP 的两个在线 Edge 节点
            let same_isp_edges: Vec<_> = self
                .nodes
                .iter()
                .filter(|n| n.node_type == NodeType::Edge && n.isp == client_isp && n.is_online)
                .collect();

            if same_isp_edges.len() >= 2 {
                // 查找它们之间的隧道
                if let Some(tunnel) = self.tunnels.iter().find(|t| {
                    t.cqi_score < BGP_FALLBACK_SCORE_THRESHOLD
                        && same_isp_edges.iter().any(|n| n.node_id == t.from_node)
                        && same_isp_edges.iter().any(|n| n.node_id == t.to_node)
                }) {
                    return (RoutingTier::EdgeRelay, Some(tunnel.tunnel_id.clone()));
                }
            }
        }

        // 策略二：跨运营商走 BGP 核心节点中继
        let core_nodes: Vec<_> = self
            .nodes
            .iter()
            .filter(|n| n.node_type == NodeType::Core && n.is_online)
            .collect();

        if let Some(best_core) = core_nodes.first() {
            // 查找经过 Core 节点的隧道（取评分最佳的）
            if let Some(tunnel) = self
                .tunnels
                .iter()
                .filter(|t| t.from_node == best_core.node_id || t.to_node == best_core.node_id)
                .min_by(|a, b| {
                    a.cqi_score
                        .partial_cmp(&b.cqi_score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            {
                return (RoutingTier::BgpCoreRelay, Some(tunnel.tunnel_id.clone()));
            }
        }

        // 策略三：实在没有可用中继，保持 P2P 直连
        (RoutingTier::P2pDirect, None)
    }

    /// 获取当前路由策略级别
    pub fn current_routing_tier(&self) -> RoutingTier {
        self.current_tier
    }

    /// 获取所有已注册节点
    pub fn nodes(&self) -> &[NodeInfo] {
        &self.nodes
    }

    /// 获取所有在线节点
    pub fn online_nodes(&self) -> Vec<&NodeInfo> {
        self.nodes.iter().filter(|n| n.is_online).collect()
    }

    /// 获取所有隧道
    pub fn tunnels(&self) -> &[Tunnel] {
        &self.tunnels
    }

    /// 消费并清空待执行指令队列
    pub fn drain_commands(&mut self) -> Vec<SdwanCommand> {
        self.pending_commands.drain(..).collect()
    }

    /// 手动添加隧道（用于初始化阶段批量建立星型拓扑）
    pub fn add_tunnel(&mut self, tunnel: Tunnel) {
        if !self.tunnels.iter().any(|t| t.tunnel_id == tunnel.tunnel_id) {
            self.tunnels.push(tunnel);
        }
    }
}

impl Default for SdwanEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_edge_node(id: &str, isp: Isp, province: &str) -> NodeInfo {
        NodeInfo {
            node_id: id.to_string(),
            node_type: NodeType::Edge,
            isp,
            region: Region {
                province: province.to_string(),
                city: province.to_string(),
            },
            public_ips: vec!["1.2.3.4".to_string()],
            bandwidth_mbps: 1000,
            is_online: true,
            last_heartbeat_ms: 1000,
        }
    }

    fn make_core_node(id: &str) -> NodeInfo {
        NodeInfo {
            node_id: id.to_string(),
            node_type: NodeType::Core,
            isp: Isp::BgpMultiline,
            region: Region {
                province: "北京".to_string(),
                city: "北京".to_string(),
            },
            public_ips: vec!["10.0.0.1".to_string()],
            bandwidth_mbps: 10000,
            is_online: true,
            last_heartbeat_ms: 1000,
        }
    }

    fn make_tunnel(id: &str, from: &str, to: &str, cqi: f64) -> Tunnel {
        Tunnel {
            tunnel_id: id.to_string(),
            from_node: from.to_string(),
            to_node: to.to_string(),
            rtt_us: 10_000,
            loss_rate: 0.001,
            jitter_us: 2_000,
            fec_barrage_active: false,
            cqi_score: cqi,
        }
    }

    #[test]
    fn test_sdwan_register_and_find_same_isp_route() {
        let mut engine = SdwanEngine::new();
        engine.register_node(make_edge_node("edge-sc-ct", Isp::ChinaTelecom, "四川"));
        engine.register_node(make_edge_node("edge-gd-ct", Isp::ChinaTelecom, "广东"));
        engine.add_tunnel(make_tunnel("t1", "edge-sc-ct", "edge-gd-ct", 10.0));

        let (tier, tunnel) =
            engine.find_best_route(Isp::ChinaTelecom, Isp::ChinaTelecom);

        assert_eq!(tier, RoutingTier::EdgeRelay);
        assert_eq!(tunnel.unwrap(), "t1");
    }

    #[test]
    fn test_sdwan_cross_isp_falls_back_to_bgp() {
        let mut engine = SdwanEngine::new();
        engine.register_node(make_edge_node("edge-sc-ct", Isp::ChinaTelecom, "四川"));
        engine.register_node(make_edge_node("edge-bj-cu", Isp::ChinaUnicom, "北京"));
        engine.register_node(make_core_node("core-bgp-bj"));
        engine.add_tunnel(make_tunnel("t-core", "core-bgp-bj", "edge-sc-ct", 15.0));

        let (tier, tunnel) =
            engine.find_best_route(Isp::ChinaTelecom, Isp::ChinaUnicom);

        assert_eq!(tier, RoutingTier::BgpCoreRelay);
        assert!(tunnel.is_some());
    }

    #[test]
    fn test_sdwan_fec_barrage_triggers_on_loss() {
        let mut engine = SdwanEngine::new();
        engine.add_tunnel(make_tunnel("t1", "a", "b", 10.0));

        // 模拟丢包率超过阈值
        engine.update_tunnel_probe("t1", 20_000, 0.01, 3_000);

        let tunnel = engine.tunnels().iter().find(|t| t.tunnel_id == "t1").unwrap();
        assert!(tunnel.fec_barrage_active);

        let cmds = engine.drain_commands();
        assert!(cmds.iter().any(|c| matches!(c, SdwanCommand::EnableFecBarrage { .. })));
    }

    #[test]
    fn test_sdwan_node_timeout() {
        let mut engine = SdwanEngine::new();
        engine.register_node(make_edge_node("edge-1", Isp::ChinaTelecom, "四川"));

        // 模拟 15 秒后检测超时
        engine.check_timeouts(1000 + NODE_OFFLINE_TIMEOUT_MS + 5000);

        assert!(!engine.nodes()[0].is_online);
    }
}
