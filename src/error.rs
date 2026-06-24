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
    /// URL 中的凭据（user:pass@）已在构造时脱敏，避免日志泄露。
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

    /// 数据库操作失败（连接、schema 校验、写入等）。
    #[error("[错误] 数据库操作失败：{detail}")]
    Database { detail: String },

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

    /// 对 URL 中的凭据做脱敏处理：移除 `user[:pass]@` 部分。
    ///
    /// 防止日志中泄露嵌入在 URL 中的用户名/密码
    /// （如 `http://user:pass@prometheus:9090` → `http://prometheus:9090`，
    ///   `http://user@prometheus:9090` → `http://prometheus:9090`）。
    pub fn redact_url(url: &str) -> String {
        // 查找 :// 后的 user[:pass]@ 部分
        // 只要 @ 出现在 :// 之后，即认为是 userinfo（RFC 3986），全部脱敏。
        // IPv6 地址在 URL 中使用方括号（[::1]）不含 @，因此不会误判。
        if let Some(scheme_end) = url.find("://") {
            let rest = &url[scheme_end + 3..];
            if let Some(at_pos) = rest.find('@') {
                return format!("{}://{}", &url[..scheme_end], &rest[at_pos + 1..]);
            }
        }
        url.to_string()
    }

    /// 对 reqwest 错误消息中的 URL 进行脱敏，防止凭据泄露。
    /// reqwest 在连接失败/超时等场景会将完整请求 URL 嵌入错误文本，
    /// 后者可能包含 `http://user:pass@host:port` 格式的凭据。
    pub fn redact_url_in_error_text(text: &str, original_url: &str, redacted_url: &str) -> String {
        text.replace(original_url, redacted_url)
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

    #[test]
    fn redact_url_strips_credentials() {
        assert_eq!(
            AppError::redact_url("http://user:pass@prometheus:9090"),
            "http://prometheus:9090"
        );
    }

    #[test]
    fn redact_url_preserves_url_without_credentials() {
        assert_eq!(
            AppError::redact_url("http://192.168.1.100:9090"),
            "http://192.168.1.100:9090"
        );
    }

    #[test]
    fn redact_url_preserves_https() {
        assert_eq!(
            AppError::redact_url("https://user:secret@prometheus.example.com/prom"),
            "https://prometheus.example.com/prom"
        );
    }

    #[test]
    fn redact_url_preserves_ipv6_without_credentials() {
        // IPv6 地址含冒号但无 @，不应被误脱敏
        assert_eq!(
            AppError::redact_url("http://[::1]:9090"),
            "http://[::1]:9090"
        );
    }

    #[test]
    fn redact_url_strips_username_only_credentials() {
        // 仅用户名（无密码）的凭据也应被脱敏
        assert_eq!(
            AppError::redact_url("http://admin@prometheus:9090"),
            "http://prometheus:9090"
        );
    }
}
