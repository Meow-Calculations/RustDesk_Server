// =============================================================================
// Route - 智能链路探针与自动多路优选模块
// =============================================================================
//
// 核心职责：
//   1. 管理多条候选链路（P2P / 中继 A / 中继 B / LAN 备份）
//   2. 对每条链路发送 UDP 探针，收集 RTT、Jitter、Loss 数据
//   3. 使用 EMA 综合评分矩阵实时排名
//   4. 当备用链路得分远优于当前主链路时，触发无感热切换
//
// 兼容性说明：
//   本模块独立于传输层实现（不直接发包），仅提供"应该切换到哪条链路"的决策。
//   实际的网络 IO 由 RustDesk 原生的 rendezvous_mediator / relay 层执行。

use crate::common::{LinkQuality, HANDOVER_SCORE_THRESHOLD, PROBE_INTERVAL_MS};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// EMA 平滑因子（链路评分用）
const LINK_EMA_ALPHA: f64 = 0.25;

/// RTT 权重（评分公式中）
const WEIGHT_RTT: f64 = 0.4;

/// Jitter 权重
const WEIGHT_JITTER: f64 = 0.3;

/// Loss 权重
const WEIGHT_LOSS: f64 = 0.3;

/// 链路超时判定阈值（毫秒）：超过此时间无响应视为断联
#[allow(dead_code)]
const LINK_TIMEOUT_MS: u64 = 5_000;

// ---------------------------------------------------------------------------
// 探针采样数据
// ---------------------------------------------------------------------------

/// 单次链路探针采样结果
#[derive(Debug, Clone)]
pub struct ProbeSample {
    /// 链路唯一标识
    pub link_id: String,
    /// 本次探测 RTT（微秒）
    pub rtt_us: u64,
    /// 本次探测抖动（微秒）
    pub jitter_us: u64,
    /// 本次探测丢包率 (0.0 ~ 1.0)
    pub loss_rate: f64,
    /// 采样时间戳（Unix 毫秒）
    pub timestamp_ms: u64,
}

// ---------------------------------------------------------------------------
// 路由决策引擎
// ---------------------------------------------------------------------------

/// 多路优选路由引擎
pub struct RouteEngine {
    /// 所有候选链路的质量快照
    links: Vec<LinkQuality>,
    /// 当前激活的主链路 ID
    active_link_id: String,
}

impl RouteEngine {
    /// 创建路由引擎
    ///
    /// # 参数
    /// - `initial_link_id`: 初始（默认）主链路 ID
    pub fn new(initial_link_id: String) -> Self {
        Self {
            links: Vec::new(),
            active_link_id: initial_link_id,
        }
    }

    /// 注册一条候选链路
    ///
    /// # 参数
    /// - `link_id`: 链路唯一标识（如 "p2p"、"relay-cn-east"）
    pub fn register_link(&mut self, link_id: String) {
        if !self.links.iter().any(|l| l.link_id == link_id) {
            self.links.push(LinkQuality {
                link_id,
                smoothed_rtt_us: 0,
                jitter_us: 0,
                smoothed_loss: 0.0,
                score: f64::MAX, // 初始评分设为最差，等待探针数据更新
            });
        }
    }

    /// 注销一条候选链路
    pub fn unregister_link(&mut self, link_id: &str) {
        self.links.retain(|l| l.link_id != link_id);
    }

    /// 输入一个探针采样结果，更新对应链路的 EMA 评分
    ///
    /// # 返回值
    /// - `Some(new_link_id)`: 触发了链路切换，返回新的最优链路 ID
    /// - `None`: 当前主链路仍然最优，不切换
    pub fn on_probe(&mut self, sample: ProbeSample) -> Option<String> {
        // 找到对应链路并更新
        if let Some(link) = self.links.iter_mut().find(|l| l.link_id == sample.link_id) {
            Self::ema_update_link(link, &sample);
        } else {
            // 未注册的链路，自动注册
            let mut new_link = LinkQuality {
                link_id: sample.link_id.clone(),
                smoothed_rtt_us: sample.rtt_us,
                jitter_us: sample.jitter_us,
                smoothed_loss: sample.loss_rate,
                score: f64::MAX,
            };
            Self::ema_update_link(&mut new_link, &sample);
            self.links.push(new_link);
        }

        // 检查是否需要切换
        self.evaluate_handover()
    }

    /// 获取当前所有链路质量快照（按评分升序排列，越低越好）
    pub fn link_rankings(&self) -> Vec<LinkQuality> {
        let mut sorted = self.links.clone();
        sorted.sort_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));
        sorted
    }

    /// 获取当前激活的主链路 ID
    pub fn active_link(&self) -> &str {
        &self.active_link_id
    }

    /// 获取推荐的探针发送间隔（毫秒）
    pub fn probe_interval_ms(&self) -> u64 {
        PROBE_INTERVAL_MS
    }

    /// 强制切换到指定链路（外部手动干预）
    pub fn force_switch(&mut self, link_id: String) {
        self.active_link_id = link_id;
    }

    // -----------------------------------------------------------------------
    // 内部实现
    // -----------------------------------------------------------------------

    /// 使用 EMA 更新链路质量指标并重算综合评分
    fn ema_update_link(link: &mut LinkQuality, sample: &ProbeSample) {
        let alpha = LINK_EMA_ALPHA;

        if link.smoothed_rtt_us == 0 {
            // 首次初始化
            link.smoothed_rtt_us = sample.rtt_us;
            link.jitter_us = sample.jitter_us;
            link.smoothed_loss = sample.loss_rate;
        } else {
            link.smoothed_rtt_us = ((1.0 - alpha) * link.smoothed_rtt_us as f64
                + alpha * sample.rtt_us as f64) as u64;
            link.jitter_us = ((1.0 - alpha) * link.jitter_us as f64
                + alpha * sample.jitter_us as f64) as u64;
            link.smoothed_loss =
                (1.0 - alpha) * link.smoothed_loss + alpha * sample.loss_rate;
        }

        // 综合评分公式：
        //   score = W_rtt * (rtt_ms) + W_jitter * (jitter_ms) + W_loss * (loss_rate * 1000)
        //   分数越低越好
        let rtt_ms = link.smoothed_rtt_us as f64 / 1000.0;
        let jitter_ms = link.jitter_us as f64 / 1000.0;
        let loss_penalty = link.smoothed_loss * 1000.0;

        link.score = WEIGHT_RTT * rtt_ms + WEIGHT_JITTER * jitter_ms + WEIGHT_LOSS * loss_penalty;
    }

    /// 评估是否需要触发链路热切换
    fn evaluate_handover(&mut self) -> Option<String> {
        if self.links.len() < 2 {
            return None;
        }

        // 获取当前主链路的评分
        let active_score = self
            .links
            .iter()
            .find(|l| l.link_id == self.active_link_id)
            .map(|l| l.score)
            .unwrap_or(f64::MAX);

        // 找到评分最低（最优）的备选链路
        let best = self
            .links
            .iter()
            .filter(|l| l.link_id != self.active_link_id)
            .min_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal));

        if let Some(best_link) = best {
            // 只有当备用链路的得分显著优于主链路时才触发切换（防止频繁震荡）
            if active_score - best_link.score > HANDOVER_SCORE_THRESHOLD {
                let new_id = best_link.link_id.clone();
                log::info!(
                    "RouteEngine: 链路热切换 [{} (score={:.1})] -> [{} (score={:.1})]",
                    self.active_link_id,
                    active_score,
                    new_id,
                    best_link.score
                );
                self.active_link_id = new_id.clone();
                return Some(new_id);
            }
        }

        None
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_sample(link_id: &str, rtt_us: u64, jitter_us: u64, loss: f64) -> ProbeSample {
        ProbeSample {
            link_id: link_id.to_string(),
            rtt_us,
            jitter_us,
            loss_rate: loss,
            timestamp_ms: 1000,
        }
    }

    #[test]
    fn test_route_register_and_probe() {
        let mut engine = RouteEngine::new("p2p".to_string());
        engine.register_link("p2p".to_string());
        engine.register_link("relay-a".to_string());

        // P2P 探针回报
        engine.on_probe(make_sample("p2p", 10_000, 2000, 0.001));
        // 中继 A 探针回报
        engine.on_probe(make_sample("relay-a", 30_000, 5000, 0.01));

        let rankings = engine.link_rankings();
        assert_eq!(rankings.len(), 2);
        // P2P 应排名第一（评分更低）
        assert_eq!(rankings[0].link_id, "p2p");
    }

    #[test]
    fn test_route_handover_on_significant_difference() {
        let mut engine = RouteEngine::new("p2p".to_string());
        engine.register_link("p2p".to_string());
        engine.register_link("relay-a".to_string());

        // P2P 质量极差
        for _ in 0..10 {
            engine.on_probe(make_sample("p2p", 500_000, 100_000, 0.20));
        }
        // 中继 A 质量极好
        for _ in 0..10 {
            engine.on_probe(make_sample("relay-a", 10_000, 2_000, 0.001));
        }

        // 此时中继 A 应该远优于 P2P，触发切换
        let result = engine.on_probe(make_sample("relay-a", 10_000, 2_000, 0.001));
        // 检查切换或者已经切换
        assert!(
            result.is_some() || engine.active_link() == "relay-a",
            "应该已经切换到 relay-a"
        );
    }

    #[test]
    fn test_route_no_handover_when_similar() {
        let mut engine = RouteEngine::new("p2p".to_string());
        engine.register_link("p2p".to_string());
        engine.register_link("relay-a".to_string());

        // 两条链路质量接近
        for _ in 0..10 {
            engine.on_probe(make_sample("p2p", 20_000, 3_000, 0.01));
            engine.on_probe(make_sample("relay-a", 22_000, 3_500, 0.012));
        }

        // 不应该频繁切换
        assert_eq!(engine.active_link(), "p2p");
    }

    #[test]
    fn test_route_auto_register_unknown_link() {
        let mut engine = RouteEngine::new("p2p".to_string());
        // 不手动注册 relay-b，直接发探针
        engine.on_probe(make_sample("relay-b", 15_000, 2_000, 0.005));
        assert_eq!(engine.link_rankings().len(), 1);
        assert_eq!(engine.link_rankings()[0].link_id, "relay-b");
    }
}
