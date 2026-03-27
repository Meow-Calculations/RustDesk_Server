// =============================================================================
// SADE (Saliency-Aware Dynamic Encoding) - 视觉显著性动态编码分发模块
// =============================================================================
//
// 核心职责：
//   根据鼠标焦点坐标和活动窗口边界，生成高斯衰减的 ROI 掩码，
//   并输出宏块级 QP 偏移指令，供编码器层实现"焦点区高画质 + 边缘区高压缩"。
//
// 兼容性说明：
//   输出的 `RoiDirective` 可直接映射到 H.264/H.265 的 ROI 编码参数，
//   无需修改 RustDesk 原生的 `video_service.rs` 主循环。

use crate::common::{FocusPoint, NetworkGrade, RoiDirective, CPU_ENCODE_DEADLINE_US};

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 默认焦点清晰区半径（像素）
const DEFAULT_FOCUS_RADIUS: u32 = 300;

/// 极端弱网 / CPU 瓶颈时的收缩焦点半径（像素）
const SHRUNK_FOCUS_RADIUS: u32 = 150;

/// 焦点区 QP 偏移（负值 = 更高画质）
const FOCUS_QP_OFFSET: i8 = -6;

/// 普通背景 QP 偏移
const BG_QP_OFFSET_NORMAL: i8 = 8;

/// 极端弱网下的背景 QP 偏移（更激进地降低画质以保帧率）
const BG_QP_OFFSET_AGGRESSIVE: i8 = 16;

// ---------------------------------------------------------------------------
// SADE 核心引擎
// ---------------------------------------------------------------------------

/// SADE 引擎，负责生成每一帧的 ROI 编码指令
pub struct SadeEngine {
    /// 画面宽度（像素）
    frame_width: u32,
    /// 画面高度（像素）
    frame_height: u32,
    /// 最近一次上报的焦点坐标
    last_focus: Option<FocusPoint>,
    /// 当前网络质量等级（由 Z-HCC 反馈）
    network_grade: NetworkGrade,
    /// 是否处于 CPU 瓶颈模式（由 capture 模块反馈）
    cpu_bottleneck: bool,
}

impl SadeEngine {
    /// 创建 SADE 引擎实例
    ///
    /// # 参数
    /// - `width`: 画面宽度（像素）
    /// - `height`: 画面高度（像素）
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            frame_width: width,
            frame_height: height,
            last_focus: None,
            network_grade: NetworkGrade::Good,
            cpu_bottleneck: false,
        }
    }

    /// 更新鼠标焦点坐标（由客户端以 60Hz 上报）
    pub fn update_focus(&mut self, focus: FocusPoint) {
        self.last_focus = Some(focus);
    }

    /// 接收来自 Z-HCC 模块的网络质量反馈
    pub fn update_network_grade(&mut self, grade: NetworkGrade) {
        self.network_grade = grade;
    }

    /// 接收来自 capture 模块的 CPU 瓶颈状态反馈
    ///
    /// # 参数
    /// - `encode_time_us`: 上一帧的 CPU 编码耗时（微秒）
    pub fn update_cpu_status(&mut self, encode_time_us: u64) {
        self.cpu_bottleneck = encode_time_us > CPU_ENCODE_DEADLINE_US;
    }

    /// 画面分辨率发生变化时调用
    pub fn update_resolution(&mut self, width: u32, height: u32) {
        self.frame_width = width;
        self.frame_height = height;
    }

    /// 生成当前帧的 ROI 编码指令
    ///
    /// 当没有焦点信息时，返回 `None`，此时编码器应使用全局均匀 QP。
    ///
    /// # 返回值
    /// - `Some(RoiDirective)`: 包含焦点区坐标、尺寸和 QP 偏移
    /// - `None`: 无焦点信息，使用默认均匀编码
    pub fn generate_roi(&self) -> Option<RoiDirective> {
        let focus = self.last_focus?;

        // 根据当前压力等级决定焦点半径和背景 QP
        let (radius, bg_qp) = self.compute_adaptive_params();

        // 以焦点坐标为中心，计算清晰区的矩形边界（Clamp 到画面范围内）
        let half = radius;
        let focus_x = focus.x.saturating_sub(half);
        let focus_y = focus.y.saturating_sub(half);
        let focus_width = (half * 2).min(self.frame_width.saturating_sub(focus_x));
        let focus_height = (half * 2).min(self.frame_height.saturating_sub(focus_y));

        Some(RoiDirective {
            focus_x,
            focus_y,
            focus_width,
            focus_height,
            focus_qp_offset: FOCUS_QP_OFFSET,
            background_qp_offset: bg_qp,
        })
    }

    /// 根据网络状况和 CPU 负载自适应地调整焦点区半径与背景压缩强度
    ///
    /// # 策略
    /// - 网络极差 / CPU 瓶颈：缩小焦点区，暴力压缩背景
    /// - 网络正常：使用标准焦点区大小
    fn compute_adaptive_params(&self) -> (u32, i8) {
        // 场景一：CPU 瓶颈模式 —— 最高优先级收缩
        if self.cpu_bottleneck {
            return (SHRUNK_FOCUS_RADIUS, BG_QP_OFFSET_AGGRESSIVE);
        }

        // 场景二：根据网络质量梯度调整
        match self.network_grade {
            NetworkGrade::Excellent | NetworkGrade::Good => {
                (DEFAULT_FOCUS_RADIUS, BG_QP_OFFSET_NORMAL)
            }
            NetworkGrade::Fair => {
                // 中等网络：略微收缩焦点区
                (DEFAULT_FOCUS_RADIUS * 3 / 4, BG_QP_OFFSET_NORMAL + 2)
            }
            NetworkGrade::Poor => {
                // 较差网络：明显收缩焦点区，加大背景压缩
                (SHRUNK_FOCUS_RADIUS, BG_QP_OFFSET_AGGRESSIVE - 4)
            }
            NetworkGrade::Critical => {
                // 极差网络：极限收缩，背景几乎变马赛克
                (SHRUNK_FOCUS_RADIUS * 2 / 3, BG_QP_OFFSET_AGGRESSIVE)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sade_generate_roi_with_focus() {
        let mut engine = SadeEngine::new(1920, 1080);
        engine.update_focus(FocusPoint {
            x: 960,
            y: 540,
            timestamp_ms: 1000,
        });

        let roi = engine.generate_roi();
        assert!(roi.is_some());

        let directive = roi.unwrap();
        // 焦点区矩形应在画面范围内
        assert!(directive.focus_x + directive.focus_width <= 1920);
        assert!(directive.focus_y + directive.focus_height <= 1080);
        // 焦点区应该是高画质（负 QP 偏移）
        assert!(directive.focus_qp_offset < 0);
        // 背景区应该是低画质（正 QP 偏移）
        assert!(directive.background_qp_offset > 0);
    }

    #[test]
    fn test_sade_no_focus_returns_none() {
        let engine = SadeEngine::new(1920, 1080);
        assert!(engine.generate_roi().is_none());
    }

    #[test]
    fn test_sade_cpu_bottleneck_shrinks_focus() {
        let mut engine = SadeEngine::new(1920, 1080);
        engine.update_focus(FocusPoint {
            x: 960,
            y: 540,
            timestamp_ms: 1000,
        });
        // 模拟 CPU 编码耗时 20ms > 16ms 上限
        engine.update_cpu_status(20_000);

        let roi = engine.generate_roi().unwrap();
        // CPU 瓶颈时焦点区宽度应更小
        assert!(roi.focus_width <= SHRUNK_FOCUS_RADIUS * 2);
        // 背景 QP 应是激进压缩
        assert_eq!(roi.background_qp_offset, BG_QP_OFFSET_AGGRESSIVE);
    }

    #[test]
    fn test_sade_critical_network_shrinks_further() {
        let mut engine = SadeEngine::new(1920, 1080);
        engine.update_focus(FocusPoint {
            x: 960,
            y: 540,
            timestamp_ms: 1000,
        });
        engine.update_network_grade(NetworkGrade::Critical);

        let roi = engine.generate_roi().unwrap();
        // 极差网络：焦点区应极限收缩
        assert!(roi.focus_width <= SHRUNK_FOCUS_RADIUS * 2);
    }
}
