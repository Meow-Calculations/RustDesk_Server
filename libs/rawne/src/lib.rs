// =============================================================================
// RAWNE (RustDesk Advanced Weak-Network Engine) - 次世代弱网远控核心引擎 V2
// =============================================================================
//
// 本 crate 实现白皮书中定义的八大核心模块：
//
//   ── V1 基础层 ──
//   1. SADE   - 视觉显著性动态编码分发
//   2. Z-HCC  - 零缓冲混合拥塞控制
//   3. E-FEC  - 弹性前向纠错
//   4. 零拷贝管线辅助 (capture)
//   5. 智能链路探针与多路优选 (route)
//
//   ── V2 演进层 ──
//   6. SVD    - 智能虚拟屏驱动管理（超级屏 / 隐私屏 / Z-HCC 联动）
//   7. SD-WAN - 多节点隧道编排（边缘+BGP核心 / 三级路由回退 / FEC弹药倾泻）
//
// 所有模块对外暴露纯 Rust API，兼容 RustDesk 原生数据结构。

/// SADE - 视觉显著性动态编码分发模块
pub mod sade;

/// Z-HCC - 零缓冲混合拥塞控制模块
pub mod zhcc;

/// E-FEC - 弹性前向纠错模块（需启用 `fec` feature）
#[cfg(feature = "fec")]
pub mod efec;

/// 零拷贝捕获管线辅助模块（CPU 回退策略）
pub mod capture;

/// 智能链路探针与自动多路优选模块
pub mod route;

/// SVD - 智能虚拟屏驱动管理模块（超级屏 / 隐私屏 / Z-HCC 联动刷新率调控）
pub mod svd;

/// SD-WAN - 多节点隧道编排与路由调度模块（边缘+BGP核心三级路由回退）
pub mod sdwan;

/// 公共数据结构与常量
pub mod common;
