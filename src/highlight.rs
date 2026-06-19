//! 阈值染色规则模块（PRD §2.6）。
//!
//! 定义 8 个触发器（核心/显存 × 均值/峰值 × 高于/低于），给定一行
//! [`CardRecord`] 计算命中的"报表列名 → 颜色"映射。本模块只产出染色决策，
//! 不触碰 Excel——渲染由 reporter 消费，从而规则演进不影响渲染层。

use crate::error::AppError;
use crate::processor::CardRecord;
use serde::{Deserialize, Serialize};

/// 报表列名常量（与 reporter 的基础列保持一致）。
pub const COL_CORE_AVG: &str = "核心利用率平均值";
pub const COL_CORE_PEAK: &str = "核心利用率峰值";
pub const COL_MEM_AVG: &str = "显存占用率平均值";
pub const COL_MEM_PEAK: &str = "显存占用率峰值";

/// HEX 颜色包装类型，反序列化时校验合法性（`#RRGGBB` 或 `#RGB`）。
///
/// 内部始终存储为 `#RRGGBB`（7 字符大写）——短格式 `#RGB` 在 `parse` 阶段
/// 自动展开为 `#RRGGBB`，保证下游消费方（如 reporter 的 `u32::from_str_radix`）
/// 无需关心短格式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HexColor(String);

impl HexColor {
    /// 校验并构造；非法返回 [`AppError::InvalidColor`]。
    /// `trigger` 参数仅用于错误提示上下文。
    ///
    /// # Errors
    ///
    /// 返回 [`AppError::InvalidColor`] 当颜色不是合法 HEX 格式。
    #[must_use = "parsing a hex color always returns a Result that should be checked"]
    pub fn parse(raw: &str, trigger: &str) -> Result<Self, AppError> {
        let s = raw.trim().to_ascii_uppercase();
        // #RGB → #RRGGBB：短格式自动展开，保证下游（如 reporter）只需处理 7 字符形式。
        let expanded = if s.starts_with('#') && s.len() == 4 && s[1..].chars().all(|c| c.is_ascii_hexdigit()) {
            format!("#{}{}{}{}{}{}", &s[1..2], &s[1..2], &s[2..3], &s[2..3], &s[3..4], &s[3..4])
        } else {
            s
        };
        if expanded.starts_with('#')
            && expanded.len() == 7
            && expanded[1..].chars().all(|c| c.is_ascii_hexdigit())
        {
            Ok(Self(expanded))
        } else {
            Err(AppError::InvalidColor {
                trigger: trigger.into(),
                raw: raw.into(),
            })
        }
    }
}

impl HexColor {
    /// 返回内部 HEX 颜色字符串（保证为 `#RRGGBB` 7 字符大写格式）。
    #[must_use]
    pub fn value(&self) -> &str {
        &self.0
    }
}

impl serde::Serialize for HexColor {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for HexColor {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s, "<配置>").map_err(serde::de::Error::custom)
    }
}

/// 单个触发器配置。`enabled: false` 则整体跳过。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TriggerConfig {
    pub enabled: bool,
    /// 0–100 的阈值。
    pub threshold: f64,
    pub color: HexColor,
}

/// 8 个触发器的显式集合；`None` 字段 = 该触发器未配置/关闭。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ThresholdTriggers {
    #[serde(default)]
    pub core_avg_above: Option<TriggerConfig>,
    #[serde(default)]
    pub core_avg_below: Option<TriggerConfig>,
    #[serde(default)]
    pub core_peak_above: Option<TriggerConfig>,
    #[serde(default)]
    pub core_peak_below: Option<TriggerConfig>,
    #[serde(default)]
    pub mem_avg_above: Option<TriggerConfig>,
    #[serde(default)]
    pub mem_avg_below: Option<TriggerConfig>,
    #[serde(default)]
    pub mem_peak_above: Option<TriggerConfig>,
    #[serde(default)]
    pub mem_peak_below: Option<TriggerConfig>,
}

/// 一条命中结果：列名 + 颜色（借用，避免克隆）。
pub struct Hit<'a> {
    pub column: &'a str,
    pub color: &'a HexColor,
}

impl ThresholdTriggers {
    /// 评估一行记录，返回命中的染色列表。
    ///
    /// 顺序遵循字段声明顺序；同一列若被多个触发器命中，取**首个**命中
    /// （由 [`first_hit`] 实现，above 优先于 below）。None/enabled:false/字段为 None
    /// 均跳过。比较为严格 `>`/`<`（不含等于）。
    #[must_use]
    pub fn evaluate_row<'a>(&'a self, r: &CardRecord) -> Vec<Hit<'a>> {
        let mut hits = Vec::new();
        if let Some(h) = first_hit(
            self.core_avg_above.as_ref(),
            self.core_avg_below.as_ref(),
            r.core_avg,
            COL_CORE_AVG,
        ) {
            hits.push(h);
        }
        if let Some(h) = first_hit(
            self.core_peak_above.as_ref(),
            self.core_peak_below.as_ref(),
            r.core_peak,
            COL_CORE_PEAK,
        ) {
            hits.push(h);
        }
        if let Some(h) = first_hit(
            self.mem_avg_above.as_ref(),
            self.mem_avg_below.as_ref(),
            r.mem_avg,
            COL_MEM_AVG,
        ) {
            hits.push(h);
        }
        if let Some(h) = first_hit(
            self.mem_peak_above.as_ref(),
            self.mem_peak_below.as_ref(),
            r.mem_peak,
            COL_MEM_PEAK,
        ) {
            hits.push(h);
        }
        hits
    }
}

/// 对单个列：依次尝试 above / below，返回首个命中的 [`Hit`]。
/// above 优先（字段声明顺序在前）。
fn first_hit<'a>(
    above: Option<&'a TriggerConfig>,
    below: Option<&'a TriggerConfig>,
    value: Option<f64>,
    column: &'a str,
) -> Option<Hit<'a>> {
    let v = value?;
    if let Some(t) = above {
        if t.enabled && v > t.threshold {
            return Some(Hit {
                column,
                color: &t.color,
            });
        }
    }
    if let Some(t) = below {
        if t.enabled && v < t.threshold {
            return Some(Hit {
                column,
                color: &t.color,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processor::CardRecord;
    use chrono::TimeZone;
    use chrono::Utc;

    fn empty_record() -> CardRecord {
        CardRecord {
            source_name: "s".into(),
            host_ip: "1.1.1.1".into(),
            node_name: String::new(),
            card_id: "0".into(),
            device_type: "X".into(),
            namespace: String::new(),
            pod: String::new(),
            container: String::new(),
            core_avg: None,
            core_peak: None,
            core_peak_time: None,
            mem_avg: None,
            mem_peak: None,
            mem_peak_time: None,
            range_start: Utc.timestamp_opt(0, 0).unwrap(),
            range_end: Utc.timestamp_opt(60, 0).unwrap(),
        }
    }

    fn trig(enabled: bool, threshold: f64, color: &str) -> TriggerConfig {
        TriggerConfig {
            enabled,
            threshold,
            color: HexColor::parse(color, "test").unwrap(),
        }
    }

    #[test]
    fn hexcolor_accepts_rrggbb_and_rgb() {
        assert!(HexColor::parse("#FF0000", "t").is_ok());
        assert!(HexColor::parse("#F00", "t").is_ok());
        assert!(HexColor::parse("#ff00aa", "t").is_ok());
    }

    #[test]
    fn hexcolor_expands_rgb_short_form() {
        // #RGB → #RRGGBB：短格式自动展开，保证下游消费方只需处理 7 字符形式。
        let c = HexColor::parse("#F00", "t").unwrap();
        assert_eq!(c.value(), "#FF0000");
        let c = HexColor::parse("#0aB", "t").unwrap();
        assert_eq!(c.value(), "#00AABB");
    }

    #[test]
    fn hexcolor_rejects_invalid() {
        assert!(HexColor::parse("red", "t").is_err());
        assert!(HexColor::parse("#GGG", "t").is_err());
        assert!(HexColor::parse("#12345", "t").is_err());
        assert!(HexColor::parse("FF0000", "t").is_err()); // 缺 #
    }

    #[test]
    fn above_trigger_hits_when_value_greater() {
        let mut r = empty_record();
        r.core_avg = Some(85.0);
        let tr = ThresholdTriggers {
            core_avg_above: Some(trig(true, 80.0, "#FF0000")),
            ..Default::default()
        };
        let hits = tr.evaluate_row(&r);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].column, COL_CORE_AVG);
        assert_eq!(hits[0].color.value(), "#FF0000");
    }

    #[test]
    fn above_does_not_hit_at_boundary_equal() {
        let mut r = empty_record();
        r.core_avg = Some(80.0); // 等于阈值，严格 > 不命中
        let tr = ThresholdTriggers {
            core_avg_above: Some(trig(true, 80.0, "#FF0000")),
            ..Default::default()
        };
        assert!(tr.evaluate_row(&r).is_empty());
    }

    #[test]
    fn below_trigger_hits_when_value_lower() {
        let mut r = empty_record();
        r.mem_peak = Some(3.0);
        let tr = ThresholdTriggers {
            mem_peak_below: Some(trig(true, 5.0, "#FFA500")),
            ..Default::default()
        };
        let hits = tr.evaluate_row(&r);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].column, COL_MEM_PEAK);
    }

    #[test]
    fn disabled_trigger_is_skipped() {
        let mut r = empty_record();
        r.core_avg = Some(99.0);
        let tr = ThresholdTriggers {
            core_avg_above: Some(trig(false, 80.0, "#FF0000")), // 关闭
            ..Default::default()
        };
        assert!(tr.evaluate_row(&r).is_empty());
    }

    #[test]
    fn none_field_is_skipped() {
        let r = empty_record(); // core_avg = None
        let tr = ThresholdTriggers {
            core_avg_above: Some(trig(true, 80.0, "#FF0000")),
            ..Default::default()
        };
        assert!(tr.evaluate_row(&r).is_empty());
    }

    #[test]
    fn same_column_above_takes_precedence_over_below() {
        // above 与 below 同列都配且都命中，取 above（字段顺序在前）
        let mut r = empty_record();
        r.core_avg = Some(50.0);
        let tr = ThresholdTriggers {
            core_avg_above: Some(trig(true, 40.0, "#FF0000")),
            core_avg_below: Some(trig(true, 60.0, "#FFA500")),
            ..Default::default()
        };
        let hits = tr.evaluate_row(&r);
        assert_eq!(hits.len(), 1, "同列只产生一个命中");
        assert_eq!(hits[0].color.value(), "#FF0000");
    }

    #[test]
    fn multiple_columns_each_at_most_one_hit() {
        let mut r = empty_record();
        r.core_avg = Some(90.0);
        r.mem_avg = Some(2.0);
        let tr = ThresholdTriggers {
            core_avg_above: Some(trig(true, 80.0, "#FF0000")),
            mem_avg_below: Some(trig(true, 10.0, "#FFA500")),
            ..Default::default()
        };
        let hits = tr.evaluate_row(&r);
        assert_eq!(hits.len(), 2);
    }
}
