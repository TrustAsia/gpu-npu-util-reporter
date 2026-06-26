//! 日志初始化模块。
//!
//! 基于 `tracing` + `tracing-subscriber` 实现结构化日志：
//! - 控制台输出：可选级别（默认 `info`）
//! - 文件输出：可选级别（默认 `debug`），可独立开关
//! - 文件自动创建父目录

use crate::config::LogConfig;
use std::path::Path;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer;

/// 初始化日志系统。
///
/// 根据配置设置控制台和文件输出，各自独立的日志级别。
/// 文件输出使用 `tracing_appender::non_blocking` 避免阻塞异步运行时。
///
/// 返回的 `Option<Guard>` 必须在 main 中持有，否则文件写入会被丢弃。
///
/// # Panics
///
/// 当文件日志启用但无法创建日志文件且也无法创建 `/dev/null` 兜底时 panic。
#[must_use]
pub fn init_logging(cfg: &LogConfig) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let console_level = cfg.console_level.as_str();
    let file_level = cfg.file_level.as_str();

    let console_filter =
        EnvFilter::try_new(console_level).unwrap_or_else(|_| EnvFilter::new("info"));
    let file_filter = EnvFilter::try_new(file_level).unwrap_or_else(|_| EnvFilter::new("debug"));

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_filter(console_filter);

    if cfg.file_enabled {
        if let Some(parent) = Path::new(&cfg.file_path).parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    eprintln!("[警告] 无法创建日志目录 {}：{e}", parent.display());
                }
            }
        }
        let file = std::fs::File::create(&cfg.file_path).unwrap_or_else(|e| {
            eprintln!(
                "[警告] 无法创建日志文件 {}：{e}，仅使用控制台输出",
                cfg.file_path
            );
            // 跨平台 null 设备：Unix 用 /dev/null，Windows 用 NUL。
            // 如果 null 设备也无法创建（极端情况），panic 是合理的，
            // 因为这表明文件系统完全不可用。
            #[cfg(unix)]
            {
                std::fs::File::create("/dev/null").expect("无法创建 /dev/null")
            }
            #[cfg(windows)]
            {
                std::fs::File::create("NUL").expect("无法创建 NUL")
            }
            #[cfg(not(any(unix, windows)))]
            {
                std::fs::File::create("/dev/null").expect("无法创建 null 设备")
            }
        });
        let (non_blocking, guard) = tracing_appender::non_blocking(file);
        let file_layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(true)
            .with_writer(non_blocking)
            .with_filter(file_filter);

        tracing_subscriber::registry()
            .with(fmt_layer)
            .with(file_layer)
            .init();
        Some(guard)
    } else {
        tracing_subscriber::registry().with(fmt_layer).init();
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_config_default_values() {
        let cfg = LogConfig::default();
        assert!(cfg.file_enabled);
        assert_eq!(cfg.console_level, "info");
        assert_eq!(cfg.file_level, "debug");
    }
}
