//! 库入口：把各模块 pub 出去，供集成测试（`tests/`）与未来复用。
//! main.rs（bin）通过 `gpu_npu_util_reporter::` 路径引用这些模块。

/// Fallback 嵌套深度上限（配置校验和运行时递归统一使用）。
pub(crate) const MAX_FALLBACK_DEPTH: usize = 10;

pub mod config;
pub mod devices;
pub mod error;
pub mod fetcher;
pub mod highlight;
pub mod logging;
pub mod mapper;
pub mod pipeline;
pub mod processor;
pub mod reporter;
pub mod template;
pub mod time_expr;
