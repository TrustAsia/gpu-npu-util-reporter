//! 路径模板引擎模块。
//!
//! 支持在输出路径和日志路径中使用模板变量，如 `{{start}}`、`{{end}}`、
//! `{{now}}` 等。模板变量会根据解析后的绝对时间替换为实际值。
//!
//! 支持的变量（以 `start` 为例，`end`/`now` 同理）：
//! - `{{start}}` — 完整时间 `YYYY-MM-DD_HH-MM-SS`
//! - `{{start_date}}` — 日期 `YYYY-MM-DD`
//! - `{{start_time}}` — 时间 `HH-MM-SS`
//! - `{{start_year}}` / `{{start_month}}` / `{{start_day}}` — 年/月/日
//! - `{{start_hour}}` / `{{start_minute}}` / `{{start_second}}` — 时/分/秒
//!
//! 日期格式 `YYYY-MM-DD`，时间格式 `HH-MM-SS`（用 `-` 避免文件名中 `:` 在
//! Windows 上非法）。所有时间按配置的 `timezone` 渲染（默认 `Asia/Shanghai`）。

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

/// 模板变量替换上下文。
#[derive(Debug, Clone)]
pub struct TemplateContext {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub now: DateTime<Utc>,
    /// 渲染时区（IANA 名，如 `Asia/Shanghai`）。
    pub tz: Tz,
}

/// 将模板中的变量替换为实际值。
///
/// 变量格式为 `{{name}}`，如 `{{start}}` → `2026-06-18_00-00-00`。
/// 未识别的变量原样保留。所有时间按 `ctx.tz` 转换后渲染。
#[must_use]
pub fn render_template(template: &str, ctx: &TemplateContext) -> String {
    let mut result = template.to_string();

    let start_local = ctx.start.with_timezone(&ctx.tz);
    let end_local = ctx.end.with_timezone(&ctx.tz);
    let now_local = ctx.now.with_timezone(&ctx.tz);

    // 按变量名降序排列替换，避免短名先匹配导致长名被截断
    // （如 {{start_date}} 必须在 {{start}} 之前替换）
    let replacements: &[(&str, String)] = &[
        // start — 长名优先
        ("{{start_date}}", start_local.format("%Y-%m-%d").to_string()),
        ("{{start_time}}", start_local.format("%H-%M-%S").to_string()),
        ("{{start_year}}", start_local.format("%Y").to_string()),
        ("{{start_month}}", start_local.format("%m").to_string()),
        ("{{start_day}}", start_local.format("%d").to_string()),
        ("{{start_hour}}", start_local.format("%H").to_string()),
        ("{{start_minute}}", start_local.format("%M").to_string()),
        ("{{start_second}}", start_local.format("%S").to_string()),
        (
            "{{start}}",
            start_local.format("%Y-%m-%d_%H-%M-%S").to_string(),
        ),
        // end
        ("{{end_date}}", end_local.format("%Y-%m-%d").to_string()),
        ("{{end_time}}", end_local.format("%H-%M-%S").to_string()),
        ("{{end_year}}", end_local.format("%Y").to_string()),
        ("{{end_month}}", end_local.format("%m").to_string()),
        ("{{end_day}}", end_local.format("%d").to_string()),
        ("{{end_hour}}", end_local.format("%H").to_string()),
        ("{{end_minute}}", end_local.format("%M").to_string()),
        ("{{end_second}}", end_local.format("%S").to_string()),
        ("{{end}}", end_local.format("%Y-%m-%d_%H-%M-%S").to_string()),
        // now
        ("{{now_date}}", now_local.format("%Y-%m-%d").to_string()),
        ("{{now_time}}", now_local.format("%H-%M-%S").to_string()),
        ("{{now_year}}", now_local.format("%Y").to_string()),
        ("{{now_month}}", now_local.format("%m").to_string()),
        ("{{now_day}}", now_local.format("%d").to_string()),
        ("{{now_hour}}", now_local.format("%H").to_string()),
        ("{{now_minute}}", now_local.format("%M").to_string()),
        ("{{now_second}}", now_local.format("%S").to_string()),
        ("{{now}}", now_local.format("%Y-%m-%d_%H-%M-%S").to_string()),
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
        // UTC 时间：2024-06-23 16:00:00 / 2024-06-24 16:00:00 / 2024-06-24 03:33:20
        // 北京时间：2024-06-24 00:00:00 / 2024-06-25 00:00:00 / 2024-06-24 11:33:20
        TemplateContext {
            start: Utc.timestamp_opt(1_719_158_400, 0).unwrap(),
            end: Utc.timestamp_opt(1_719_244_800, 0).unwrap(),
            now: Utc.timestamp_opt(1_719_200_000, 0).unwrap(),
            tz: "Asia/Shanghai".parse().unwrap(),
        }
    }

    #[test]
    fn render_start_variable() {
        let result = render_template("report-{{start}}.xlsx", &ctx());
        // UTC 16:00 → 北京时间次日 00:00
        assert_eq!(result, "report-2024-06-24_00-00-00.xlsx");
    }

    #[test]
    fn render_end_variable() {
        let result = render_template("report-{{end}}.xlsx", &ctx());
        assert_eq!(result, "report-2024-06-25_00-00-00.xlsx");
    }

    #[test]
    fn render_date_only() {
        let result = render_template("report-{{start_date}}.xlsx", &ctx());
        assert_eq!(result, "report-2024-06-24.xlsx");
    }

    #[test]
    fn render_time_only() {
        let result = render_template("report-{{start_time}}.xlsx", &ctx());
        assert_eq!(result, "report-00-00-00.xlsx");
    }

    #[test]
    fn render_now_variable() {
        let result = render_template("log/{{now}}.log", &ctx());
        assert_eq!(result, "log/2024-06-24_11-33-20.log");
    }

    #[test]
    fn render_multiple_variables() {
        let result = render_template("{{start_date}}_{{end_date}}_report.xlsx", &ctx());
        assert_eq!(result, "2024-06-24_2024-06-25_report.xlsx");
    }

    #[test]
    fn render_year_month_day() {
        let result = render_template("{{start_year}}/{{start_month}}/{{start_day}}", &ctx());
        assert_eq!(result, "2024/06/24");
    }

    #[test]
    fn render_hour_minute_second() {
        // now = 北京时间 11:33:20
        let result = render_template("{{now_hour}}:{{now_minute}}:{{now_second}}", &ctx());
        assert_eq!(result, "11:33:20");
    }

    #[test]
    fn timezone_utc() {
        let mut ctx = ctx();
        ctx.tz = "UTC".parse().unwrap();
        let result = render_template("{{start}}", &ctx);
        // UTC 模式下应显示原始 UTC 时间
        assert_eq!(result, "2024-06-23_16-00-00");
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
