// =============================================================================
// E-FEC (Elastic Forward Error Correction) - 弹性前向纠错模块
// =============================================================================
//
// 核心职责：
//   根据实时丢包率动态调整 Reed-Solomon 冗余比率，
//   对关键帧 (I-Frame) 施加最高保护等级，对 P/B 帧施加弹性保护。
//   配合动态 Jitter Buffer，在接收端免重传恢复丢失数据包。
//
// 兼容性说明：
//   使用 `reed-solomon-erasure` crate 的 GF(2^8) 实现。
//   API 纯 Rust，可被 hbb_common 或 video_service 直接调用。

use reed_solomon_erasure::galois_8::ReedSolomon;

use crate::common::DEFAULT_FEC_REDUNDANCY;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 最大冗余比率上限（防止带宽完全被 FEC 包填满）
const MAX_REDUNDANCY: f64 = 0.6;

/// 最小冗余比率下限（即使网络极好也保留少量保险）
const MIN_REDUNDANCY: f64 = 0.05;

/// I 帧强制最低冗余率（关键帧绝对不能丢）
const I_FRAME_MIN_REDUNDANCY: f64 = 0.3;

/// Jitter Buffer 默认深度（帧数）
const DEFAULT_JITTER_BUFFER_DEPTH: usize = 3;

/// Jitter Buffer 最大深度（帧数）
const MAX_JITTER_BUFFER_DEPTH: usize = 6;

// ---------------------------------------------------------------------------
// 帧类型枚举
// ---------------------------------------------------------------------------

/// 视频帧类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    /// 关键帧（I-Frame）
    IFrame,
    /// 预测帧（P-Frame / B-Frame）
    PFrame,
}

// ---------------------------------------------------------------------------
// FEC 编码参数
// ---------------------------------------------------------------------------

/// FEC 编码参数快照（由引擎计算后输出给发送端）
#[derive(Debug, Clone)]
pub struct FecParams {
    /// 数据分片数（k）
    pub data_shards: usize,
    /// 冗余分片数（m）
    pub parity_shards: usize,
    /// 实际冗余比率 (m / k)
    pub redundancy_ratio: f64,
}

// ---------------------------------------------------------------------------
// E-FEC 核心引擎
// ---------------------------------------------------------------------------

/// E-FEC 弹性前向纠错引擎
pub struct EfecEngine {
    /// 当前弹性冗余比率
    current_redundancy: f64,
    /// Jitter Buffer 深度（帧数）
    jitter_buffer_depth: usize,
}

impl EfecEngine {
    /// 创建 E-FEC 引擎实例
    pub fn new() -> Self {
        Self {
            current_redundancy: DEFAULT_FEC_REDUNDANCY,
            jitter_buffer_depth: DEFAULT_JITTER_BUFFER_DEPTH,
        }
    }

    /// 根据当前丢包率更新冗余策略
    ///
    /// # 参数
    /// - `loss_rate`: 当前 EMA 平滑后的丢包率 (0.0 ~ 1.0)
    pub fn update_loss_rate(&mut self, loss_rate: f64) {
        // 冗余比率 = 丢包率 × 2.5 的弹性系数（提供 2.5 倍安全余量）
        // 但必须钳制在 [MIN, MAX] 之间
        let target = (loss_rate * 2.5).clamp(MIN_REDUNDANCY, MAX_REDUNDANCY);
        // 使用 EMA 平滑过渡，避免冗余率剧烈跳动
        self.current_redundancy = 0.3 * target + 0.7 * self.current_redundancy;

        // 根据丢包率自适应 Jitter Buffer 深度
        self.jitter_buffer_depth = if loss_rate > 0.10 {
            MAX_JITTER_BUFFER_DEPTH
        } else if loss_rate > 0.05 {
            DEFAULT_JITTER_BUFFER_DEPTH + 1
        } else {
            DEFAULT_JITTER_BUFFER_DEPTH
        };
    }

    /// 为指定帧类型计算 FEC 编码参数
    ///
    /// # 参数
    /// - `frame_type`: 帧类型（I 帧 / P 帧）
    /// - `data_shards`: 原始数据分片数（由帧大小和 MTU 决定）
    ///
    /// # 返回值
    /// - `FecParams`: 包含数据分片数、冗余分片数和实际冗余比率
    pub fn compute_fec_params(&self, frame_type: FrameType, data_shards: usize) -> FecParams {
        let redundancy = match frame_type {
            FrameType::IFrame => {
                // I 帧采用更高的保护等级
                self.current_redundancy.max(I_FRAME_MIN_REDUNDANCY)
            }
            FrameType::PFrame => self.current_redundancy,
        };

        let parity_shards = ((data_shards as f64) * redundancy).ceil() as usize;
        // 至少保证 1 个冗余分片
        let parity_shards = parity_shards.max(1);

        FecParams {
            data_shards,
            parity_shards,
            redundancy_ratio: parity_shards as f64 / data_shards as f64,
        }
    }

    /// 对原始数据分片执行 Reed-Solomon 编码，生成冗余分片
    ///
    /// # 参数
    /// - `shards`: 可变引用的分片数组。前 `data_count` 个是原始数据，
    ///   后面的冗余分片必须预分配好内存（填零即可），本函数会原地填写。
    /// - `data_count`: 原始数据分片数
    ///
    /// # 返回值
    /// - `Ok(())`: 编码成功
    /// - `Err(String)`: 编码失败（参数不合法等）
    pub fn encode_shards(
        &self,
        shards: &mut [Vec<u8>],
        data_count: usize,
    ) -> Result<(), String> {
        let parity_count = shards.len().checked_sub(data_count)
            .ok_or_else(|| format!(
                "数据分片数 ({}) 超过总分片数 ({})，参数非法",
                data_count, shards.len()
            ))?;
        if parity_count == 0 {
            return Err("冗余分片数不能为 0".to_string());
        }

        let rs = ReedSolomon::new(data_count, parity_count)
            .map_err(|e| format!("Reed-Solomon 初始化失败: {:?}", e))?;

        rs.encode(shards)
            .map_err(|e| format!("Reed-Solomon 编码失败: {:?}", e))?;

        Ok(())
    }

    /// 在接收端使用 Reed-Solomon 解码恢复丢失的分片
    ///
    /// # 参数
    /// - `shards`: 分片数组，丢失的分片用 `None` 表示
    /// - `data_count`: 原始数据分片数
    ///
    /// # 返回值
    /// - `Ok(())`: 恢复成功，`shards` 中的 `None` 已被填回
    /// - `Err(String)`: 恢复失败（丢包超出纠错能力）
    pub fn recover_shards(
        &self,
        shards: &mut Vec<Option<Vec<u8>>>,
        data_count: usize,
    ) -> Result<(), String> {
        let total = shards.len();
        let parity_count = total - data_count;

        let rs = ReedSolomon::new(data_count, parity_count)
            .map_err(|e| format!("Reed-Solomon 初始化失败: {:?}", e))?;

        rs.reconstruct(shards)
            .map_err(|e| format!("Reed-Solomon 恢复失败（丢包超出纠错上限）: {:?}", e))?;

        Ok(())
    }

    /// 获取当前 Jitter Buffer 推荐深度（帧数）
    pub fn jitter_buffer_depth(&self) -> usize {
        self.jitter_buffer_depth
    }

    /// 获取当前弹性冗余比率
    pub fn current_redundancy(&self) -> f64 {
        self.current_redundancy
    }
}

impl Default for EfecEngine {
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

    #[test]
    fn test_efec_i_frame_high_protection() {
        let engine = EfecEngine::new();
        let params = engine.compute_fec_params(FrameType::IFrame, 10);
        // I 帧冗余率应 >= 30%
        assert!(params.redundancy_ratio >= I_FRAME_MIN_REDUNDANCY);
        assert!(params.parity_shards >= 3);
    }

    #[test]
    fn test_efec_loss_rate_adjusts_redundancy() {
        let mut engine = EfecEngine::new();

        // 低丢包
        engine.update_loss_rate(0.01);
        let low_loss_redundancy = engine.current_redundancy();

        // 高丢包
        engine.update_loss_rate(0.15);
        engine.update_loss_rate(0.15); // 多次更新让 EMA 收敛
        engine.update_loss_rate(0.15);
        let high_loss_redundancy = engine.current_redundancy();

        // 高丢包时冗余应更大
        assert!(high_loss_redundancy > low_loss_redundancy);
    }

    #[test]
    fn test_efec_encode_recover() {
        let engine = EfecEngine::new();
        let data_count = 5;
        let parity_count = 3;
        let shard_size = 64;

        // 构造原始数据分片
        let mut shards: Vec<Vec<u8>> = (0..data_count)
            .map(|i| vec![i as u8; shard_size])
            .collect();
        // 追加空白冗余分片
        for _ in 0..parity_count {
            shards.push(vec![0u8; shard_size]);
        }

        // 编码
        engine.encode_shards(&mut shards, data_count).unwrap();

        // 模拟丢失第 1 和第 3 个数据分片
        let mut recovery: Vec<Option<Vec<u8>>> = shards.into_iter().map(Some).collect();
        recovery[1] = None;
        recovery[3] = None;

        // 恢复
        engine.recover_shards(&mut recovery, data_count).unwrap();

        // 验证恢复后的数据正确性
        assert_eq!(recovery[1].as_ref().unwrap(), &vec![1u8; shard_size]);
        assert_eq!(recovery[3].as_ref().unwrap(), &vec![3u8; shard_size]);
    }

    #[test]
    fn test_efec_jitter_buffer_depth() {
        let mut engine = EfecEngine::new();
        engine.update_loss_rate(0.01);
        assert_eq!(engine.jitter_buffer_depth(), DEFAULT_JITTER_BUFFER_DEPTH);

        engine.update_loss_rate(0.12);
        assert_eq!(engine.jitter_buffer_depth(), MAX_JITTER_BUFFER_DEPTH);
    }
}
