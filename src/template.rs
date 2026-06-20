//! 路径模板引擎模块。
//!
//! 支持在输出路径和日志路径中使用模板变量，如 `{{start}}`、`{{end}}`、
//! `{{now}}` 等。模板变量会根据解析后的绝对时间替换为实际值。
//!
//! 支持的变量：
//! - `{{start}}` / `{{start_date}}` / `{{start_time}}` — 查询起始时间
//! - `{{end}}` / `{{end_date}}` / `{{end_time}}` — 查询结束时间
//! - `{{now}}` / `{{now_date}}` / `{{now_time}}` — 当前运行时刻
//! - `{{source}}` — 数据源别名（仅在单源场景有意义）
//!
//! 日期格式 `YYYY-MM-DD`，时间格式 `HH-MM-SS`（用 `-` 避免文件名中 `:` 在
//! Windows 上非法），完整时间格式 `YYYY-MM-DD_HH-MM-SS`。

use chrono::{DateTime, Utc};

/// 模板变量替换上下文。
#[derive(Debug, Clone)]
pub struct TemplateContext {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub now: DateTime<Utc>,
}

/// 将模板中的变量替换为实际值。
///
/// 变量格式为 `{{name}}`，如 `{{start}}` → `2026-06-18_00-00-00`。
/// 未识别的变量原样保留。
#[must_use]
pub fn render_template(template: &str, ctx: &TemplateContext) -> String {
    let mut result = template.to_string();
    // 按变量名排序替换，避免短名先匹配导致长名被截断
    let replacements: &[(&str, String)] = &[
        ("{{start_date}}", ctx.start.format("%Y-%m-%d").to_string()),
        ("{{start_time}}", ctx.start.format("%H-%M-%S").to_string()),
        ("{{start}}", ctx.start.format("%Y-%m-%d_%H-%M-%S").to_string()),
        ("{{end_date}}", ctx.end.format("%Y-%m-%d").to_string()),
        ("{{end_time}}", ctx.end.format("%H-%M-%S").to_string()),
        ("{{end}}", ctx.end.format("%Y-%m-%d_%H-%M-%S").to_string()),
        ("{{now_date}}", ctx.now.format("%Y-%m-%d").to_string()),
        ("{{now_time}}", ctx.now.format("%H-%M-%S").to_string()),
        ("{{now}}", ctx.now.format("%Y-%m-%d_%H-%M-%S").to_string()),
    ];
    for (var, val) in replacements {
        result = result.replace(var, val);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ctx() -> TemplateContext {
        TemplateContext {
            start: Utc.timestamp_opt(1_719_158_400, 0).unwrap(), // 2024-06-23 16:00:00 UTC
            end: Utc.timestamp_opt(1_719_244_800, 0).unwrap(),   // 2024-06-24 16:00:00 UTC
            now: Utc.timestamp_opt(1_719_200_000, 0).unwrap(),   // 2024-06-24 03:33:20 UTC
        }
    }

    #[test]
    fn render_start_variable() {
        let result = render_template("report-{{start}}.xlsx", &ctx());
        assert_eq!(result, "report-2024-06-23_16-00-00.xlsx");
    }

    #[test]
    fn render_end_variable() {
        let result = render_template("report-{{end}}.xlsx", &ctx());
        assert_eq!(result, "report-2024-06-24_16-00-00.xlsx");
    }

    #[test]
    fn render_date_only() {
        let result = render_template("report-{{start_date}}.xlsx", &ctx());
        assert_eq!(result, "report-2024-06-23.xlsx");
    }

    #[test]
    fn render_time_only() {
        let result = render_template("report-{{start_time}}.xlsx", &ctx());
        assert_eq!(result, "report-16-00-00.xlsx");
    }

    #[test]
    fn render_now_variable() {
        let result = render_template("log/{{now}}.log", &ctx());
        assert_eq!(result, "log/2024-06-24_03-33-20.log");
    }

    #[test]
    fn render_multiple_variables() {
        let result = render_template("{{start_date}}_{{end_date}}_report.xlsx", &ctx());
        assert_eq!(result, "2024-06-23_2024-06-24_report.xlsx");
    }

    #[test]
    fn unknown_variables_preserved() {
        let result = render_template("{{unknown}}_report.xlsx", &ctx());
        assert_eq!(result, "{{unknown}}_report.xlsx");
    }

    #[test]
    fn no_variables_preserved() {
        let result = render_template("plain.xlsx", &ctx());
        assert_eq!(result, "plain.xlsx");
    }
}
