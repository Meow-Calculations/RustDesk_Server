// =============================================================================
// SVD (Smart Virtual Display) - 智能虚拟屏驱动管理模块
// =============================================================================
//
// 核心职责：
//   1. 根据主控端上报的 EDID 信息（分辨率 / 刷新率 / HDR），在被控端动态
//      生成 1:1 点对点的虚拟显示器实例（IddCx / Indirect Display Driver）。
//   2. 实现"超级屏"模式：将桌面所有窗口迁移至虚拟显示器，同时熄灭物理屏幕。
//   3. 与 Z-HCC 拥塞控制联动：网络恶化时反向调控虚拟屏刷新率，降低 GPU 渲染负载。
//   4. 提供 Graceful Teardown 机制：远控断开或崩溃时自动销毁虚拟显示器。
//
// 兼容性说明：
//   本模块为纯逻辑层，抽象了平台无关的虚拟屏管理接口。
//   底层 IddCx 驱动交互由平台层 (`platform::win_svd`) 实现。

use serde::{Deserialize, Serialize};

use crate::common::NetworkGrade;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 虚拟屏最大支持数量（每个远控会话内）
const MAX_VIRTUAL_DISPLAYS: usize = 3;

/// 默认刷新率（Hz），当主控端未指定时使用
const DEFAULT_REFRESH_RATE: u32 = 60;

/// 最大支持刷新率（Hz）
const MAX_REFRESH_RATE: u32 = 240;

/// 优雅销毁超时时间（毫秒）：超过此时间强制销毁
const GRACEFUL_TEARDOWN_TIMEOUT_MS: u64 = 3_000;

/// 网络恢复后刷新率回升步进（Hz）
#[allow(dead_code)]
const REFRESH_RATE_RECOVERY_STEP: u32 = 15;

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

/// 主控端 EDID 信息（由握手协议传递）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientEdid {
    /// 主控端屏幕宽度（像素）
    pub width: u32,
    /// 主控端屏幕高度（像素）
    pub height: u32,
    /// 目标刷新率（Hz）
    pub refresh_rate: u32,
    /// 是否支持 HDR
    pub hdr_enabled: bool,
    /// 色彩空间标识（如 "sRGB"、"DCI-P3"、"HDR10"）
    pub color_space: String,
}

/// 虚拟显示器实例快照
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualDisplay {
    /// 虚拟显示器唯一 ID（由驱动层分配）
    pub display_id: u32,
    /// 水平分辨率（像素）
    pub width: u32,
    /// 垂直分辨率（像素）
    pub height: u32,
    /// 当前活跃刷新率（Hz）
    pub refresh_rate: u32,
    /// 是否开启 HDR
    pub hdr_enabled: bool,
    /// 是否为超级屏模式（独占模式，物理屏已熄灭）
    pub is_super_display: bool,
    /// 创建时间戳（Unix 毫秒）
    pub created_at_ms: u64,
}

/// 物理屏幕电源状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PhysicalScreenPower {
    /// 物理屏幕正常亮起
    On,
    /// 物理屏幕已切断信号（隐私模式）
    Off,
    /// 物理屏幕低功耗待机
    Standby,
}

/// SVD 引擎输出的控制指令（下发给平台层驱动执行）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SvdCommand {
    /// 创建虚拟显示器
    CreateDisplay {
        width: u32,
        height: u32,
        refresh_rate: u32,
        hdr_enabled: bool,
    },
    /// 销毁虚拟显示器
    DestroyDisplay { display_id: u32 },
    /// 修改虚拟显示器刷新率（Z-HCC 联动）
    SetRefreshRate {
        display_id: u32,
        refresh_rate: u32,
    },
    /// 切换物理屏幕电源状态（隐私屏控制）
    SetPhysicalPower { power: PhysicalScreenPower },
    /// 将桌面窗口迁移至虚拟显示器（超级屏模式）
    MigrateDesktop { target_display_id: u32 },
    /// 将桌面窗口回迁至物理显示器（优雅销毁流程）
    RestoreDesktop,
}

// ---------------------------------------------------------------------------
// SVD 核心引擎
// ---------------------------------------------------------------------------

/// SVD 智能虚拟屏管理引擎
pub struct SvdEngine {
    /// 当前活跃的虚拟显示器列表
    displays: Vec<VirtualDisplay>,
    /// 主控端握手协商的 EDID 信息
    client_edid: Option<ClientEdid>,
    /// 物理屏幕当前电源状态
    physical_power: PhysicalScreenPower,
    /// 待执行的指令队列（由上层消费并下发给驱动层）
    pending_commands: Vec<SvdCommand>,
    /// 下一个虚拟显示器 ID 分配计数器
    next_display_id: u32,
    /// 当前网络质量等级（由 Z-HCC 更新）
    network_grade: NetworkGrade,
}

impl SvdEngine {
    /// 创建 SVD 引擎实例
    pub fn new() -> Self {
        Self {
            displays: Vec::new(),
            client_edid: None,
            physical_power: PhysicalScreenPower::On,
            pending_commands: Vec::new(),
            next_display_id: 1,
            network_grade: NetworkGrade::Good,
        }
    }

    /// 握手阶段：接收主控端 EDID 信息，并生成点对点虚拟显示器
    ///
    /// # 参数
    /// - `edid`: 主控端上报的屏幕 EDID 参数
    ///
    /// # 返回值
    /// - `Ok(VirtualDisplay)`: 新增的虚拟显示器快照
    /// - `Err(String)`: 创建失败原因
    pub fn on_client_connect(&mut self, edid: ClientEdid) -> Result<VirtualDisplay, String> {
        if self.displays.len() >= MAX_VIRTUAL_DISPLAYS {
            return Err(format!(
                "虚拟显示器数量已达上限 ({})，拒绝创建新实例",
                MAX_VIRTUAL_DISPLAYS
            ));
        }

        let refresh = edid.refresh_rate.clamp(1, MAX_REFRESH_RATE);
        let display_id = self.next_display_id;
        self.next_display_id += 1;

        let display = VirtualDisplay {
            display_id,
            width: edid.width,
            height: edid.height,
            refresh_rate: refresh,
            hdr_enabled: edid.hdr_enabled,
            is_super_display: false,
            created_at_ms: Self::now_ms(),
        };

        // 下发创建指令
        self.pending_commands.push(SvdCommand::CreateDisplay {
            width: edid.width,
            height: edid.height,
            refresh_rate: refresh,
            hdr_enabled: edid.hdr_enabled,
        });

        self.displays.push(display.clone());
        self.client_edid = Some(edid);

        log::info!(
            "SVD: 创建虚拟显示器 #{} ({}x{} @{}Hz, HDR={})",
            display_id,
            display.width,
            display.height,
            display.refresh_rate,
            display.hdr_enabled,
        );

        Ok(display)
    }

    /// 开启"超级屏"模式：
    ///   1. 将桌面窗口全部迁移至指定虚拟显示器
    ///   2. 切断所有物理屏幕信号（真·隐私屏）
    ///
    /// # 参数
    /// - `display_id`: 目标虚拟显示器 ID
    pub fn enable_super_display(&mut self, display_id: u32) -> Result<(), String> {
        let display = self
            .displays
            .iter_mut()
            .find(|d| d.display_id == display_id)
            .ok_or_else(|| format!("虚拟显示器 #{} 不存在", display_id))?;

        display.is_super_display = true;

        // 指令序列：先迁移桌面，再熄灭物理屏
        self.pending_commands.push(SvdCommand::MigrateDesktop {
            target_display_id: display_id,
        });
        self.pending_commands.push(SvdCommand::SetPhysicalPower {
            power: PhysicalScreenPower::Off,
        });
        self.physical_power = PhysicalScreenPower::Off;

        log::info!("SVD: 超级屏模式已激活 (显示器 #{}), 物理屏已熄灭", display_id);
        Ok(())
    }

    /// 关闭"超级屏"模式，恢复物理屏幕
    pub fn disable_super_display(&mut self) {
        for display in &mut self.displays {
            display.is_super_display = false;
        }
        self.pending_commands.push(SvdCommand::RestoreDesktop);
        self.pending_commands.push(SvdCommand::SetPhysicalPower {
            power: PhysicalScreenPower::On,
        });
        self.physical_power = PhysicalScreenPower::On;

        log::info!("SVD: 超级屏模式已关闭, 物理屏已恢复");
    }

    /// Z-HCC 联动接口：根据网络质量动态调整虚拟屏刷新率
    ///
    /// 该方法实现"网络感知型渲染计算"——
    /// 网络恶化时降低虚拟屏刷新率，从源头减少 GPU 渲染量。
    pub fn on_network_grade_change(&mut self, grade: NetworkGrade) {
        self.network_grade = grade;

        let target_refresh = match grade {
            NetworkGrade::Excellent => MAX_REFRESH_RATE.min(
                self.client_edid
                    .as_ref()
                    .map(|e| e.refresh_rate)
                    .unwrap_or(DEFAULT_REFRESH_RATE),
            ),
            NetworkGrade::Good => 60,
            NetworkGrade::Fair => 45,
            NetworkGrade::Poor => 30,
            NetworkGrade::Critical => 15,
        };

        // 对所有活跃虚拟显示器下发刷新率修改指令
        for display in &mut self.displays {
            if display.refresh_rate != target_refresh {
                log::info!(
                    "SVD: Z-HCC 联动 — 显示器 #{} 刷新率 {}Hz -> {}Hz (网络: {:?})",
                    display.display_id,
                    display.refresh_rate,
                    target_refresh,
                    grade,
                );
                display.refresh_rate = target_refresh;
                self.pending_commands.push(SvdCommand::SetRefreshRate {
                    display_id: display.display_id,
                    refresh_rate: target_refresh,
                });
            }
        }
    }

    /// 优雅销毁：远控会话结束或异常断开时，安全回收所有虚拟资源
    ///
    /// # 执行序列
    /// 1. 恢复物理屏幕信号
    /// 2. 将所有窗口回迁到物理屏幕
    /// 3. 逐个销毁虚拟显示器
    pub fn graceful_teardown(&mut self) {
        log::info!(
            "SVD: 执行优雅销毁 ({}ms 超时), 共 {} 个虚拟显示器",
            GRACEFUL_TEARDOWN_TIMEOUT_MS,
            self.displays.len(),
        );

        // 步骤一：恢复物理屏幕
        if self.physical_power != PhysicalScreenPower::On {
            self.pending_commands.push(SvdCommand::SetPhysicalPower {
                power: PhysicalScreenPower::On,
            });
            self.physical_power = PhysicalScreenPower::On;
        }

        // 步骤二：回迁桌面
        self.pending_commands.push(SvdCommand::RestoreDesktop);

        // 步骤三：销毁所有虚拟显示器
        for display in &self.displays {
            self.pending_commands.push(SvdCommand::DestroyDisplay {
                display_id: display.display_id,
            });
        }
        self.displays.clear();
        self.client_edid = None;
    }

    /// 获取当前待执行的指令队列（消费后清空）
    pub fn drain_commands(&mut self) -> Vec<SvdCommand> {
        self.pending_commands.drain(..).collect()
    }

    /// 获取所有当前活跃的虚拟显示器快照
    pub fn active_displays(&self) -> &[VirtualDisplay] {
        &self.displays
    }

    /// 获取当前物理屏幕电源状态
    pub fn physical_screen_power(&self) -> PhysicalScreenPower {
        self.physical_power
    }

    // -----------------------------------------------------------------------
    // 内部工具
    // -----------------------------------------------------------------------

    /// 获取当前时间戳（Unix 毫秒）
    fn now_ms() -> u64 {
        instant::Instant::now().elapsed().as_millis() as u64
    }
}

impl Default for SvdEngine {
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

    fn test_edid() -> ClientEdid {
        ClientEdid {
            width: 2560,
            height: 1600,
            refresh_rate: 144,
            hdr_enabled: true,
            color_space: "HDR10".to_string(),
        }
    }

    #[test]
    fn test_svd_create_display_on_connect() {
        let mut engine = SvdEngine::new();
        let result = engine.on_client_connect(test_edid());
        assert!(result.is_ok());

        let display = result.unwrap();
        assert_eq!(display.width, 2560);
        assert_eq!(display.height, 1600);
        assert_eq!(display.refresh_rate, 144);
        assert!(display.hdr_enabled);
        assert!(!display.is_super_display);
        assert_eq!(engine.active_displays().len(), 1);
    }

    #[test]
    fn test_svd_max_displays_limit() {
        let mut engine = SvdEngine::new();
        for _ in 0..MAX_VIRTUAL_DISPLAYS {
            engine.on_client_connect(test_edid()).unwrap();
        }
        // 超限应失败
        let result = engine.on_client_connect(test_edid());
        assert!(result.is_err());
    }

    #[test]
    fn test_svd_super_display_enables_privacy() {
        let mut engine = SvdEngine::new();
        let display = engine.on_client_connect(test_edid()).unwrap();
        engine.enable_super_display(display.display_id).unwrap();

        assert_eq!(engine.physical_screen_power(), PhysicalScreenPower::Off);
        assert!(engine.active_displays()[0].is_super_display);
    }

    #[test]
    fn test_svd_network_grade_throttles_refresh() {
        let mut engine = SvdEngine::new();
        engine.on_client_connect(test_edid()).unwrap();

        // 网络极差时应降频
        engine.on_network_grade_change(NetworkGrade::Critical);
        assert_eq!(engine.active_displays()[0].refresh_rate, 15);

        // 网络恢复时应提升
        engine.on_network_grade_change(NetworkGrade::Excellent);
        assert_eq!(engine.active_displays()[0].refresh_rate, 144);
    }

    #[test]
    fn test_svd_graceful_teardown() {
        let mut engine = SvdEngine::new();
        engine.on_client_connect(test_edid()).unwrap();
        engine.enable_super_display(1).unwrap();
        engine.drain_commands(); // 清空之前的指令

        engine.graceful_teardown();

        assert!(engine.active_displays().is_empty());
        assert_eq!(engine.physical_screen_power(), PhysicalScreenPower::On);

        let commands = engine.drain_commands();
        // 应包含：恢复物理屏 + 回迁桌面 + 销毁显示器
        assert!(commands.len() >= 3);
    }
}
