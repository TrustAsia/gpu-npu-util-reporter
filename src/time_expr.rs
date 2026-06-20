//! 相对时间表达式解析模块。
//!
//! 支持在配置文件和 CLI 参数中使用相对时间表达式，如 `now`、`now-7d`、
//! `end-7d`、`start+3h` 等。表达式基于一个上下文（`now`、`start`、`end`）
//! 计算，产出绝对时间字符串（`YYYY-MM-DD HH:MM:SS`）。
//!
//! 语法：
//! ```text
//! expr := anchor ( offset )?
//! anchor := "now" | "start" | "end"
//! offset := ("+" | "-") <number> <unit>
//! unit := "y" | "M" | "d" | "h" | "m" | "s"
//! ```
//!
//! 示例：
//! - `now` → 当前时间
//! - `now-7d` → 7 天前
//! - `end-7d` → 查询结束时间前 7 天
//! - `start+3h` → 查询开始时间后 3 小时
//! - `2026-06-18 00:00:00` → 绝对时间，原样返回

use crate::error::AppError;
use chrono::{DateTime, Duration, NaiveDateTime, Utc};

/// 相对时间解析的上下文：提供 `now`、`start`、`end` 三个锚点。
#[derive(Debug, Clone)]
pub struct TimeContext {
    /// 当前时刻。
    pub now: DateTime<Utc>,
    /// 查询时间范围起点。
    pub start: Option<DateTime<Utc>>,
    /// 查询时间范围终点。
    pub end: Option<DateTime<Utc>>,
}

impl Default for TimeContext {
    fn default() -> Self {
        Self {
            now: Utc::now(),
            start: None,
            end: None,
        }
    }
}

/// 解析时间表达式，返回绝对时间。
///
/// 如果输入是标准的绝对时间格式（`YYYY-MM-DD HH:MM:SS`），直接解析返回。
/// 如果是相对时间表达式（如 `now-7d`），基于 `ctx` 计算后返回。
///
/// # Errors
///
/// 返回 [`AppError::TimeFormat`] 当表达式语法错误或引用了未提供的锚点。
pub fn resolve_time_expr(expr: &str, ctx: &TimeContext) -> Result<DateTime<Utc>, AppError> {
    let trimmed = expr.trim();

    // 先尝试绝对时间
    if let Ok(dt) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S") {
        return Ok(DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc));
    }

    // 相对时间：解析 anchor
    let (anchor, rest) = parse_anchor(trimmed)?;
    let base = match anchor {
        "now" => ctx.now,
        "start" => ctx.start.ok_or_else(|| AppError::TimeFormat {
            raw: format!("{trimmed}：引用了 start，但 start 尚未解析"),
        })?,
        "end" => ctx.end.ok_or_else(|| AppError::TimeFormat {
            raw: format!("{trimmed}：引用了 end，但 end 尚未解析"),
        })?,
        _ => unreachable!(),
    };

    // 解析偏移量（可选）
    if rest.is_empty() {
        return Ok(base);
    }
    let offset = parse_offset(rest)?;
    Ok(base + offset)
}

/// 解析锚点关键字，返回 (`anchor_name`, 剩余部分)。
fn parse_anchor(s: &str) -> Result<(&str, &str), AppError> {
    for anchor in ["now", "start", "end"] {
        if let Some(rest) = s.strip_prefix(anchor) {
            // 确保锚点后紧跟偏移运算符或结束
            if rest.is_empty() || rest.starts_with('+') || rest.starts_with('-') {
                return Ok((anchor, rest));
            }
        }
    }
    Err(AppError::TimeFormat {
        raw: format!(
            "「{s}」不是合法的时间表达式（需为绝对时间 YYYY-MM-DD HH:MM:SS 或相对时间 now/start/end[+/-<N><unit>]）"
        ),
    })
}

/// 解析偏移量，如 `-7d`、`+3h`。
fn parse_offset(s: &str) -> Result<Duration, AppError> {
    if s.is_empty() {
        return Ok(Duration::zero());
    }
    let sign: i64 = if s.starts_with('-') {
        -1
    } else if s.starts_with('+') {
        1
    } else {
        return Err(AppError::TimeFormat {
            raw: format!("偏移量应以 + 或 - 开头（当前：{s}）"),
        });
    };
    let rest = &s[1..];

    // 解析数值 + 单位：支持连续多段，如 1d12h
    let mut total = Duration::zero();
    let mut pos = 0;
    let bytes = rest.as_bytes();
    while pos < bytes.len() {
        // 读取数字部分
        let num_start = pos;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            pos += 1;
        }
        if pos == num_start {
            return Err(AppError::TimeFormat {
                raw: format!("偏移量中缺少数字（当前：{s}）"),
            });
        }
        let num: i64 = rest[num_start..pos]
            .parse()
            .map_err(|_| AppError::TimeFormat {
                raw: format!("偏移量中的数字无效（当前：{s}）"),
            })?;
        // 读取单位
        if pos >= bytes.len() {
            return Err(AppError::TimeFormat {
                raw: format!("偏移量中缺少单位（当前：{s}）"),
            });
        }
        let unit = bytes[pos] as char;
        pos += 1;
        let duration = match unit {
            'y' => Duration::days(num * 365), // 近似：1年 = 365天
            'M' => Duration::days(num * 30),  // 近似：1月 = 30天
            'd' => Duration::days(num),
            'h' => Duration::hours(num),
            'm' => Duration::minutes(num),
            's' => Duration::seconds(num),
            _ => {
                return Err(AppError::TimeFormat {
                    raw: format!(
                        "偏移量单位「{unit}」不合法（支持：y=年, M=月, d=天, h=时, m=分, s=秒）"
                    ),
                })
            }
        };
        total += duration;
    }
    let result = if sign < 0 { -total } else { total };
    Ok(result)
}

/// 判断字符串是否为相对时间表达式（而非绝对时间）。
#[must_use]
pub fn is_relative_time(s: &str) -> bool {
    let trimmed = s.trim();
    trimmed.starts_with("now") || trimmed.starts_with("start") || trimmed.starts_with("end")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ctx() -> TimeContext {
        TimeContext {
            now: Utc.timestamp_opt(1_719_200_000, 0).unwrap(), // 2024-06-24 08:53:20 UTC
            start: Some(Utc.timestamp_opt(1_719_158_400, 0).unwrap()), // 2024-06-23 21:00:00
            end: Some(Utc.timestamp_opt(1_719_244_800, 0).unwrap()),   // 2024-06-24 21:00:00
        }
    }

    #[test]
    fn absolute_time_parsed_directly() {
        let result =
            resolve_time_expr("2026-06-18 00:00:00", &TimeContext::default()).unwrap();
        assert_eq!(
            result.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2026-06-18 00:00:00"
        );
    }

    #[test]
    fn now_returns_current_time() {
        let ctx = ctx();
        let result = resolve_time_expr("now", &ctx).unwrap();
        assert_eq!(result, ctx.now);
    }

    #[test]
    fn now_minus_7d() {
        let ctx = ctx();
        let result = resolve_time_expr("now-7d", &ctx).unwrap();
        assert_eq!(result, ctx.now - Duration::days(7));
    }

    #[test]
    fn end_minus_7d() {
        let ctx = ctx();
        let result = resolve_time_expr("end-7d", &ctx).unwrap();
        assert_eq!(result, ctx.end.unwrap() - Duration::days(7));
    }

    #[test]
    fn start_plus_3h() {
        let ctx = ctx();
        let result = resolve_time_expr("start+3h", &ctx).unwrap();
        assert_eq!(result, ctx.start.unwrap() + Duration::hours(3));
    }

    #[test]
    fn compound_offset() {
        let ctx = ctx();
        let result = resolve_time_expr("now-1d12h", &ctx).unwrap();
        assert_eq!(result, ctx.now - Duration::days(1) - Duration::hours(12));
    }

    #[test]
    fn start_without_end_fails() {
        let ctx = TimeContext {
            now: Utc::now(),
            start: None,
            end: None,
        };
        let result = resolve_time_expr("start+1h", &ctx);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_expression_fails() {
        let result = resolve_time_expr("tomorrow", &TimeContext::default());
        assert!(result.is_err());
    }

    #[test]
    fn invalid_unit_fails() {
        let result = resolve_time_expr("now-7w", &TimeContext::default());
        assert!(result.is_err());
    }

    #[test]
    fn is_relative_time_detects_expressions() {
        assert!(is_relative_time("now"));
        assert!(is_relative_time("now-7d"));
        assert!(is_relative_time("start+3h"));
        assert!(is_relative_time("end-1d"));
        assert!(!is_relative_time("2026-06-18 00:00:00"));
        assert!(!is_relative_time("yesterday"));
    }

    #[test]
    fn all_units() {
        let ctx = ctx();
        assert!(resolve_time_expr("now-1y", &ctx).is_ok());
        assert!(resolve_time_expr("now-1M", &ctx).is_ok());
        assert!(resolve_time_expr("now-1d", &ctx).is_ok());
        assert!(resolve_time_expr("now-1h", &ctx).is_ok());
        assert!(resolve_time_expr("now-1m", &ctx).is_ok());
        assert!(resolve_time_expr("now-1s", &ctx).is_ok());
    }
}
