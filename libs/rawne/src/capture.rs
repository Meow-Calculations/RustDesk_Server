// =============================================================================
// Capture - 零拷贝捕获管线与 CPU 回退策略模块
// =============================================================================
//
// 核心职责：
//   1. 抽象屏幕捕获管线状态（VRAM 零拷贝 / CPU DMA 映射 / GDI 回退）
//   2. 动态分辨率热降级：当 CPU 软编码耗时超标时自动 Down-scale
//   3. 编码器类型探测与自适应切换
//
// 兼容性说明：
//   本模块不直接调用平台 API（DXGI/NvFBC），而是提供决策层。
//   实际的系统调用由 RustDesk 原生的 `scrap` crate 执行，
//   本模块负责"告诉 scrap 该怎么做"。

use crate::common::{EncoderType, CPU_ENCODE_DEADLINE_US};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 连续超时帧数阈值：超过此次数触发分辨率降级
const OVERDUE_FRAME_THRESHOLD: u32 = 5;

/// 分辨率降级步进比率（例如 0.75 = 降至原来的 75%）
const DOWNSCALE_RATIO: f64 = 0.75;

/// 分辨率最低下限比率（不会降至原分辨率的 40% 以下）
const MIN_SCALE_RATIO: f64 = 0.40;

/// 连续正常帧数阈值：超过此次数后尝试恢复分辨率
const RECOVERY_FRAME_THRESHOLD: u32 = 30;

// ---------------------------------------------------------------------------
// 捕获管线状态机
// ---------------------------------------------------------------------------

/// 捕获管线决策引擎
///
/// 负责跟踪 CPU 编码耗时，决定是否触发分辨率降级或编码器回退。
pub struct CaptureEngine {
    /// 当前编码器类型
    encoder_type: EncoderType,
    /// 原始分辨率（宽）
    native_width: u32,
    /// 原始分辨率（高）
    native_height: u32,
    /// 当前缩放比率 (1.0 = 原始分辨率)
    current_scale: f64,
    /// 连续超时帧计数器
    overdue_counter: u32,
    /// 连续正常帧计数器
    normal_counter: u32,
    /// 上一帧的编码耗时（微秒）
    last_encode_time_us: u64,
}

impl CaptureEngine {
    /// 创建捕获管线决策引擎
    ///
    /// # 参数
    /// - `encoder_type`: 初始编码器类型
    /// - `native_width`: 原始捕获分辨率宽度
    /// - `native_height`: 原始捕获分辨率高度
    pub fn new(encoder_type: EncoderType, native_width: u32, native_height: u32) -> Self {
        Self {
            encoder_type,
            native_width,
            native_height,
            current_scale: 1.0,
            overdue_counter: 0,
            normal_counter: 0,
            last_encode_time_us: 0,
        }
    }

    /// 上报单帧编码耗时，驱动分辨率自适应状态机
    ///
    /// # 参数
    /// - `encode_time_us`: 本帧的编码耗时（微秒）
    ///
    /// # 返回值
    /// - `Some((width, height))`: 如果触发了分辨率变更，返回新的目标分辨率
    /// - `None`: 分辨率保持不变
    pub fn on_frame_encoded(&mut self, encode_time_us: u64) -> Option<(u32, u32)> {
        self.last_encode_time_us = encode_time_us;

        // GPU 硬件编码时，不需要分辨率降级（GPU 不会因为分辨率高而超时）
        if self.encoder_type == EncoderType::HardwareGpu {
            return None;
        }

        if encode_time_us > CPU_ENCODE_DEADLINE_US {
            // 编码超时
            self.overdue_counter += 1;
            self.normal_counter = 0;

            if self.overdue_counter >= OVERDUE_FRAME_THRESHOLD {
                // 触发降级
                return self.downscale();
            }
        } else {
            // 编码正常
            self.normal_counter += 1;
            self.overdue_counter = 0;

            if self.normal_counter >= RECOVERY_FRAME_THRESHOLD {
                // 尝试恢复分辨率
                return self.upscale();
            }
        }

        None
    }

    /// 查询当前编码器类型
    pub fn encoder_type(&self) -> EncoderType {
        self.encoder_type
    }

    /// 切换编码器类型（当硬件编码器初始化失败/恢复时由外层调用）
    pub fn set_encoder_type(&mut self, encoder_type: EncoderType) {
        self.encoder_type = encoder_type;
        // 如果切回 GPU，自动恢复到全分辨率
        if encoder_type == EncoderType::HardwareGpu {
            self.current_scale = 1.0;
            self.overdue_counter = 0;
            self.normal_counter = 0;
        }
    }

    /// 获取当前有效分辨率
    pub fn effective_resolution(&self) -> (u32, u32) {
        let w = (self.native_width as f64 * self.current_scale) as u32;
        let h = (self.native_height as f64 * self.current_scale) as u32;
        // 保证是偶数（编码器要求）
        (w & !1, h & !1)
    }

    /// 获取当前缩放比率
    pub fn current_scale(&self) -> f64 {
        self.current_scale
    }

    /// 获取上一帧的编码耗时（微秒）
    pub fn last_encode_time_us(&self) -> u64 {
        self.last_encode_time_us
    }

    /// 判断当前是否处于 CPU 瓶颈状态
    pub fn is_cpu_bottleneck(&self) -> bool {
        self.encoder_type != EncoderType::HardwareGpu
            && self.last_encode_time_us > CPU_ENCODE_DEADLINE_US
    }

    // -----------------------------------------------------------------------
    // 内部实现
    // -----------------------------------------------------------------------

    /// 执行一次分辨率降级
    fn downscale(&mut self) -> Option<(u32, u32)> {
        let new_scale = (self.current_scale * DOWNSCALE_RATIO).max(MIN_SCALE_RATIO);
        if (new_scale - self.current_scale).abs() < 0.01 {
            // 已经到最低下限，无法继续降级
            return None;
        }
        self.current_scale = new_scale;
        self.overdue_counter = 0;
        log::warn!(
            "CaptureEngine: CPU 编码超时，触发分辨率降级 -> {:.0}% ({}x{})",
            self.current_scale * 100.0,
            self.effective_resolution().0,
            self.effective_resolution().1
        );
        Some(self.effective_resolution())
    }

    /// 尝试恢复一级分辨率
    fn upscale(&mut self) -> Option<(u32, u32)> {
        if self.current_scale >= 1.0 {
            return None;
        }
        let new_scale = (self.current_scale / DOWNSCALE_RATIO).min(1.0);
        self.current_scale = new_scale;
        self.normal_counter = 0;
        log::info!(
            "CaptureEngine: CPU 编码余裕充足，恢复分辨率 -> {:.0}% ({}x{})",
            self.current_scale * 100.0,
            self.effective_resolution().0,
            self.effective_resolution().1
        );
        Some(self.effective_resolution())
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capture_gpu_no_downscale() {
        let mut engine = CaptureEngine::new(EncoderType::HardwareGpu, 1920, 1080);
        // GPU 编码即使"超时"也不降级（GPU 不会真超时）
        for _ in 0..20 {
            assert!(engine.on_frame_encoded(50_000).is_none());
        }
        assert_eq!(engine.effective_resolution(), (1920, 1080));
    }

    #[test]
    fn test_capture_cpu_downscale_on_overdue() {
        let mut engine = CaptureEngine::new(EncoderType::SoftwareCpu, 1920, 1080);
        // 连续 N 帧超时
        for _ in 0..(OVERDUE_FRAME_THRESHOLD - 1) {
            assert!(engine.on_frame_encoded(20_000).is_none());
        }
        // 第 N 帧触发降级
        let result = engine.on_frame_encoded(20_000);
        assert!(result.is_some());
        let (w, h) = result.unwrap();
        assert!(w < 1920);
        assert!(h < 1080);
    }

    #[test]
    fn test_capture_recovery_after_stable() {
        let mut engine = CaptureEngine::new(EncoderType::SoftwareCpu, 1920, 1080);
        // 先触发降级
        for _ in 0..OVERDUE_FRAME_THRESHOLD {
            engine.on_frame_encoded(20_000);
        }
        let scaled = engine.current_scale();
        assert!(scaled < 1.0);

        // 连续 N 帧正常后恢复
        for _ in 0..(RECOVERY_FRAME_THRESHOLD - 1) {
            engine.on_frame_encoded(8_000);
        }
        let result = engine.on_frame_encoded(8_000);
        assert!(result.is_some());
        assert!(engine.current_scale() > scaled);
    }

    #[test]
    fn test_capture_even_resolution() {
        let engine = CaptureEngine::new(EncoderType::SoftwareCpu, 1921, 1081);
        let (w, h) = engine.effective_resolution();
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
    }
}
