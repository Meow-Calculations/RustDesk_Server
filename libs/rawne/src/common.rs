// =============================================================================
// RAWNE 公共数据结构与常量
// =============================================================================
//
// 本模块定义了所有子模块共享的常量、枚举与核心数据结构。
// 设计原则：与 RustDesk 原生 API (video_qos, codec) 完全兼容。

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 常量区
// ---------------------------------------------------------------------------

/// 默认初始帧率 (fps)，对齐 video_qos.rs 优化后的值
pub const DEFAULT_INIT_FPS: u32 = 30;

/// 最大支持帧率 (fps)，对齐虚拟屏高刷解锁
pub const MAX_FPS: u32 = 144;

/// Z-HCC 突发探测窗口大小（帧数）
pub const BURST_WINDOW_SIZE: usize = 5;

/// Z-HCC 平稳历史窗口大小（帧数）
pub const STABLE_WINDOW_SIZE: usize = 30;

/// E-FEC 默认冗余比率（0.0 ~ 1.0），即 30% 冗余
pub const DEFAULT_FEC_REDUNDANCY: f64 = 0.3;

/// 链路探针发送间隔（毫秒）
pub const PROBE_INTERVAL_MS: u64 = 200;

/// 链路热切换的质量差值阈值 (当备用链路得分领先主链路超过此值时触发切换)
pub const HANDOVER_SCORE_THRESHOLD: f64 = 15.0;

/// CPU 软编码单帧耗时上限（微秒），超过则触发热降级
pub const CPU_ENCODE_DEADLINE_US: u64 = 16_000;

// ---------------------------------------------------------------------------
// 枚举
// ---------------------------------------------------------------------------

/// 编码器类型枚举，用于区分当前管线状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EncoderType {
    /// GPU 硬件编码（NVENC / VCE / QSV）—— VRAM 零拷贝管线
    HardwareGpu,
    /// GPU 硬件编码（通过系统内存中转）
    HardwareRam,
    /// CPU 纯软件编码 (VP9 / AV1 / AOM)
    SoftwareCpu,
}

/// 网络质量等级，由 Z-HCC 实时评估输出
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NetworkGrade {
    /// 极佳（RTT < 20ms, 丢包 < 0.5%）
    Excellent,
    /// 良好（RTT < 80ms, 丢包 < 2%）
    Good,
    /// 一般（RTT < 150ms, 丢包 < 5%）
    Fair,
    /// 较差（RTT < 300ms, 丢包 < 15%）
    Poor,
    /// 极差 / 断联风险（RTT > 300ms 或 丢包 > 15%）
    Critical,
}

// ---------------------------------------------------------------------------
// 核心数据结构
// ---------------------------------------------------------------------------

/// 鼠标焦点坐标信息，由客户端以 60Hz 频率上报
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FocusPoint {
    /// 鼠标 X 坐标（像素）
    pub x: u32,
    /// 鼠标 Y 坐标（像素）
    pub y: u32,
    /// 时间戳（Unix 毫秒）
    pub timestamp_ms: u64,
}

/// 单次 RTT 采样结果
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RttSample {
    /// 往返延迟（微秒）
    pub rtt_us: u64,
    /// 单向延迟变差 (OWD Variation)，微秒
    pub owd_variation_us: i64,
    /// 本次采样对应的丢包率 (0.0 ~ 1.0)
    pub loss_rate: f64,
    /// 采样时间戳（Unix 毫秒）
    pub timestamp_ms: u64,
}

/// 链路质量快照，供路由优选模块使用
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkQuality {
    /// 链路唯一标识（例如中继节点 ID 或 "p2p"）
    pub link_id: String,
    /// 平滑后的 RTT（微秒）
    pub smoothed_rtt_us: u64,
    /// 抖动方差 (Jitter)，微秒
    pub jitter_us: u64,
    /// 平滑后的丢包率 (0.0 ~ 1.0)
    pub smoothed_loss: f64,
    /// EMA 综合评分（分数越低越好，0 = 完美）
    pub score: f64,
}

/// SADE 输出的 ROI 编码指令
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoiDirective {
    /// 焦点清晰区的左上角 X（像素）
    pub focus_x: u32,
    /// 焦点清晰区的左上角 Y（像素）
    pub focus_y: u32,
    /// 焦点清晰区的宽度（像素）
    pub focus_width: u32,
    /// 焦点清晰区的高度（像素）
    pub focus_height: u32,
    /// 焦点区的推荐 QP 偏移量（负值 = 更高画质）
    pub focus_qp_offset: i8,
    /// 边缘区的推荐 QP 偏移量（正值 = 降低画质）
    pub background_qp_offset: i8,
}
