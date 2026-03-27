// =============================================================================
// RAWNE (RustDesk Advanced Weak-Network Engine) - 次世代弱网远控核心引擎
// =============================================================================
//
// 本 crate 实现白皮书 `core_algorithm_whitepaper.md` 中定义的五大核心模块：
//   1. SADE  - 视觉显著性动态编码分发
//   2. Z-HCC - 零缓冲混合拥塞控制
//   3. E-FEC - 弹性前向纠错
//   4. 零拷贝管线辅助 (capture)
//   5. 智能链路探针与多路优选 (route)
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

/// 公共数据结构与常量
pub mod common;
