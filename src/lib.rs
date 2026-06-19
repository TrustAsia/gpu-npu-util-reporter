//! 库入口：把各模块 pub 出去，供集成测试（`tests/`）与未来复用。
//! main.rs（bin）通过 `gpu_npu_util_reporter::` 路径引用这些模块。

pub mod config;
pub mod devices;
pub mod error;
pub mod fetcher;
pub mod highlight;
pub mod mapper;
pub mod processor;
pub mod reporter;
