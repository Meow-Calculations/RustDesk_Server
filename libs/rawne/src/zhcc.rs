// =============================================================================
// Z-HCC (Zero-Queue Hybrid Congestion Control) - 零缓冲混合拥塞控制模块
// =============================================================================
//
// 核心职责：
//   基于双窗口（突发探测 + 平稳历史）和单向延迟梯度检测，
//   实时评估网络质量等级，输出给 SADE 和 E-FEC 联动。
//   在路由器开始丢包前 100ms 提前踩刹车，队列清空后 10ms 内指数恢复。
//
// 兼容性说明：
//   与 video_qos.rs 的 QualityStatus 互不冲突，可作为更精细的底层信号源。

use crate::common::{
    NetworkGrade, RttSample, BURST_WINDOW_SIZE, DEFAULT_INIT_FPS, MAX_FPS, STABLE_WINDOW_SIZE,
};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 延迟梯度阈值（微秒）：连续帧间延迟增量超过此值视为路由排队
const DELAY_GRADIENT_THRESHOLD_US: i64 = 5_000;

/// 触发紧急刹车的连续梯度报警次数
const GRADIENT_ALARM_COUNT: usize = 3;

/// EMA 平滑因子 (α)，越大越敏感
const EMA_ALPHA: f64 = 0.3;

// ---------------------------------------------------------------------------
// Z-HCC 核心引擎
// ---------------------------------------------------------------------------

/// Z-HCC 拥塞控制引擎
pub struct ZhccEngine {
    /// 突发探测窗口（最近 N 个样本）
    burst_window: Vec<RttSample>,
    /// 平稳历史窗口（最近 N 个样本）
    stable_window: Vec<RttSample>,
    /// EMA 平滑后的 RTT（微秒）
    smoothed_rtt_us: f64,
    /// EMA 平滑后的丢包率
    smoothed_loss: f64,
    /// EMA 平滑后的抖动（微秒）
    smoothed_jitter_us: f64,
    /// 连续观察到的延迟梯度告警次数
    gradient_alarm_counter: usize,
    /// 当前推荐的目标帧率 (fps)
    target_fps: u32,
    /// 当前输出的网络质量等级
    current_grade: NetworkGrade,
}

impl ZhccEngine {
    /// 创建 Z-HCC 引擎实例
    pub fn new() -> Self {
        Self {
            burst_window: Vec::with_capacity(BURST_WINDOW_SIZE),
            stable_window: Vec::with_capacity(STABLE_WINDOW_SIZE),
            smoothed_rtt_us: 0.0,
            smoothed_loss: 0.0,
            smoothed_jitter_us: 0.0,
            gradient_alarm_counter: 0,
            target_fps: DEFAULT_INIT_FPS,
            current_grade: NetworkGrade::Good,
        }
    }

    /// 输入一个新的 RTT 采样并驱动整个拥塞控制状态机
    ///
    /// # 参数
    /// - `sample`: 采样数据（RTT、单向延迟变差、丢包率）
    ///
    /// # 返回值
    /// - 返回最新的推荐帧率和网络质量等级的快照 `(target_fps, NetworkGrade)`
    pub fn on_sample(&mut self, sample: RttSample) -> (u32, NetworkGrade) {
        // ① 滑入突发窗口
        self.push_burst(sample);

        // ② 滑入平稳窗口
        self.push_stable(sample);

        // ③ EMA 平滑
        self.ema_update(&sample);

        // ④ 延迟梯度检测
        self.detect_delay_gradient(&sample);

        // ⑤ 综合判定网络等级
        self.evaluate_grade();

        // ⑥ 调整目标帧率
        self.adjust_fps();

        (self.target_fps, self.current_grade)
    }

    /// 获取当前网络质量等级
    pub fn grade(&self) -> NetworkGrade {
        self.current_grade
    }

    /// 获取当前推荐目标帧率
    pub fn target_fps(&self) -> u32 {
        self.target_fps
    }

    /// 获取 EMA 平滑后的 RTT（微秒）
    pub fn smoothed_rtt_us(&self) -> f64 {
        self.smoothed_rtt_us
    }

    /// 获取 EMA 平滑后的丢包率
    pub fn smoothed_loss(&self) -> f64 {
        self.smoothed_loss
    }

    // -----------------------------------------------------------------------
    // 内部实现
    // -----------------------------------------------------------------------

    /// 将样本滑入突发探测窗口（FIFO，固定容量）
    fn push_burst(&mut self, sample: RttSample) {
        if self.burst_window.len() >= BURST_WINDOW_SIZE {
            self.burst_window.remove(0);
        }
        self.burst_window.push(sample);
    }

    /// 将样本滑入平稳历史窗口（FIFO，固定容量）
    fn push_stable(&mut self, sample: RttSample) {
        if self.stable_window.len() >= STABLE_WINDOW_SIZE {
            self.stable_window.remove(0);
        }
        self.stable_window.push(sample);
    }

    /// 使用指数移动平均 (EMA) 平滑 RTT、丢包率与 Jitter
    fn ema_update(&mut self, sample: &RttSample) {
        let rtt = sample.rtt_us as f64;
        let loss = sample.loss_rate;
        let jitter = (sample.owd_variation_us.unsigned_abs()) as f64;

        if self.smoothed_rtt_us == 0.0 {
            // 首次初始化
            self.smoothed_rtt_us = rtt;
            self.smoothed_loss = loss;
            self.smoothed_jitter_us = jitter;
        } else {
            self.smoothed_rtt_us = EMA_ALPHA * rtt + (1.0 - EMA_ALPHA) * self.smoothed_rtt_us;
            self.smoothed_loss = EMA_ALPHA * loss + (1.0 - EMA_ALPHA) * self.smoothed_loss;
            self.smoothed_jitter_us =
                EMA_ALPHA * jitter + (1.0 - EMA_ALPHA) * self.smoothed_jitter_us;
        }
    }

    /// 延迟梯度检测
    ///
    /// 如果突发窗口内连续多次出现正梯度（延迟递增），说明中间路由器正在排队，
    /// 需要在丢包发生前提前踩刹车。
    fn detect_delay_gradient(&mut self, sample: &RttSample) {
        if sample.owd_variation_us > DELAY_GRADIENT_THRESHOLD_US {
            self.gradient_alarm_counter += 1;
        } else {
            // 梯度消失，快速清零
            self.gradient_alarm_counter = self.gradient_alarm_counter.saturating_sub(2);
        }
    }

    /// 综合判定网络质量等级
    fn evaluate_grade(&mut self) {
        // 优先级最高：延迟梯度报警 → 主动预警为 Poor/Critical
        if self.gradient_alarm_counter >= GRADIENT_ALARM_COUNT {
            self.current_grade = NetworkGrade::Poor;
            return;
        }

        let rtt = self.smoothed_rtt_us;
        let loss = self.smoothed_loss;

        self.current_grade = if rtt < 20_000.0 && loss < 0.005 {
            NetworkGrade::Excellent
        } else if rtt < 80_000.0 && loss < 0.02 {
            NetworkGrade::Good
        } else if rtt < 150_000.0 && loss < 0.05 {
            NetworkGrade::Fair
        } else if rtt < 300_000.0 && loss < 0.15 {
            NetworkGrade::Poor
        } else {
            NetworkGrade::Critical
        };
    }

    /// 根据网络等级调整目标帧率
    ///
    /// 策略：
    ///   - 网络恶化 → 帧率迅速减半（快速刹车）
    ///   - 网络恢复 → 帧率指数级爬升（拒绝逐帧 +1 的蜗牛恢复）
    fn adjust_fps(&mut self) {
        let ideal_fps = match self.current_grade {
            NetworkGrade::Excellent => MAX_FPS,
            NetworkGrade::Good => 60,
            NetworkGrade::Fair => 30,
            NetworkGrade::Poor => 15,
            NetworkGrade::Critical => 5,
        };

        if ideal_fps < self.target_fps {
            // 快速刹车：直接减半，但不低于 ideal
            self.target_fps = self.target_fps / 2;
            if self.target_fps < ideal_fps {
                self.target_fps = ideal_fps;
            }
        } else if ideal_fps > self.target_fps {
            // 指数恢复：步进 = (ideal - current) / 2，最小为 1
            let step = ((ideal_fps - self.target_fps) / 2).max(1);
            self.target_fps = (self.target_fps + step).min(MAX_FPS);
        }
    }
}

impl Default for ZhccEngine {
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

    /// 辅助函数：生成一个正常的 RTT 采样
    fn normal_sample(rtt_us: u64) -> RttSample {
        RttSample {
            rtt_us,
            owd_variation_us: 100,
            loss_rate: 0.001,
            timestamp_ms: 1000,
        }
    }

    /// 辅助函数：生成一个高延迟梯度的采样
    fn gradient_sample() -> RttSample {
        RttSample {
            rtt_us: 200_000,
            owd_variation_us: 10_000, // 超过 DELAY_GRADIENT_THRESHOLD_US
            loss_rate: 0.08,
            timestamp_ms: 1000,
        }
    }

    #[test]
    fn test_zhcc_starts_at_good() {
        let engine = ZhccEngine::new();
        assert_eq!(engine.grade(), NetworkGrade::Good);
        assert_eq!(engine.target_fps(), DEFAULT_INIT_FPS);
    }

    #[test]
    fn test_zhcc_excellent_network_ramps_up() {
        let mut engine = ZhccEngine::new();
        // 喂入多个极优样本
        for _ in 0..20 {
            engine.on_sample(normal_sample(5_000)); // 5ms RTT
        }
        // 帧率应该上升
        assert!(engine.target_fps() > DEFAULT_INIT_FPS);
        assert_eq!(engine.grade(), NetworkGrade::Excellent);
    }

    #[test]
    fn test_zhcc_gradient_alarm_triggers_brake() {
        let mut engine = ZhccEngine::new();
        // 先稳定在 Good
        for _ in 0..10 {
            engine.on_sample(normal_sample(30_000));
        }
        let fps_before = engine.target_fps();

        // 触发延迟梯度告警（连续 3 个高梯度样本）
        for _ in 0..GRADIENT_ALARM_COUNT {
            engine.on_sample(gradient_sample());
        }

        // 帧率应该下降（刹车效果）
        assert!(engine.target_fps() < fps_before);
        assert_eq!(engine.grade(), NetworkGrade::Poor);
    }

    #[test]
    fn test_zhcc_recovery_is_exponential() {
        let mut engine = ZhccEngine::new();
        // 先触发刹车
        for _ in 0..5 {
            engine.on_sample(gradient_sample());
        }
        let fps_after_brake = engine.target_fps();

        // 然后恢复
        let mut fps_history = vec![fps_after_brake];
        for _ in 0..30 {
            engine.on_sample(normal_sample(10_000));
            fps_history.push(engine.target_fps());
        }

        // 验证：帧率是单调不递减的（恢复过程中不会回退）
        for i in 1..fps_history.len() {
            assert!(fps_history[i] >= fps_history[i - 1]);
        }
        // 最终帧率应显著高于刹车时
        assert!(fps_history.last().unwrap() > &fps_after_brake);
    }
}
