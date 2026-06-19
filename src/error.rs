//! 统一错误类型模块。
//!
//! 全程序使用 [`AppError`] 作为错误载体，配合 `thiserror` 派生
//! `Error`/`Display`，保证所有错误都带中文、对人类友好的上下文。
//! 非致命情况（单卡/单源失败）用 [`AppError::Warning`] 表达，不中断流程。

use thiserror::Error;

/// 应用统一错误类型。
///
/// 设计意图：把所有可预见的失败场景枚举化，每个变体携带足够定位的字段，
/// 其 `Display` 输出即面向终端用户的中文提示。
#[derive(Error, Debug, Clone)]
pub enum AppError {
    /// 配置文件解析或字段缺失等致命错误。
    #[error("[错误] 配置文件 {path} 解析失败：{reason}")]
    Config { path: String, reason: String },

    /// 无法连接到 Prometheus（网络层）。
    #[error(
        "[错误] 无法连接到 Prometheus 数据源 {source_name}（{url}），请检查网络或配置：{detail}"
    )]
    Prometheus {
        source_name: String,
        url: String,
        detail: String,
    },

    /// `PromQL` 查询被 Prometheus 拒绝或返回非成功状态。
    #[error("[错误] PromQL 查询返回异常（{source_name}）：{detail}")]
    Promql { source_name: String, detail: String },

    /// 时间字符串不符合 `YYYY-MM-DD HH:MM:SS`。
    #[error("[错误] 时间格式无效：{raw}，请使用 YYYY-MM-DD HH:MM:SS")]
    TimeFormat { raw: String },

    /// 阈值染色颜色不是合法 HEX。
    #[error(
        "[错误] 阈值触发器 {trigger} 的颜色 {raw} 不是合法的 HEX 颜色（需为 #RRGGBB 或 #RGB）"
    )]
    InvalidColor { trigger: String, raw: String },

    /// 资产表加载或解析失败。
    #[error("[错误] 资产表加载失败（{path}）：{detail}")]
    Mapping { path: String, detail: String },

    /// 报表写入失败（磁盘、权限等）。
    #[error("[错误] 报表写入失败：{detail}")]
    Report { detail: String },

    /// 非致命警告：仅记录、不中断。
    /// 预留给未来"单卡失败降级告警"的统一通道，当前 main 直接收集字符串。
    #[allow(dead_code)]
    #[error("[警告] {msg}")]
    Warning { msg: String },
}

impl AppError {
    /// 判断是否为非致命警告（调用方据此决定是否继续）。
    /// 预留接口，供未来告警统一化使用。
    #[must_use]
    #[allow(dead_code)]
    pub const fn is_warning(&self) -> bool {
        matches!(self, Self::Warning { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_error_displays_chinese() {
        let e = AppError::Config {
            path: "./config.yaml".into(),
            reason: "time_range.start 字段缺失".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("[错误]"));
        assert!(s.contains("./config.yaml"));
        assert!(s.contains("time_range.start"));
    }

    #[test]
    fn warning_is_non_fatal() {
        let e = AppError::Warning {
            msg: "卡片无数据".into(),
        };
        assert!(e.is_warning());
    }

    #[test]
    fn fatal_errors_are_not_warnings() {
        let e = AppError::TimeFormat { raw: "x".into() };
        assert!(!e.is_warning());
    }
}
