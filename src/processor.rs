//! 数据处理与聚合模块。
//!
//! 把 fetcher 拉回的时序点（`Series`）聚合成一张卡在时间范围内的统计结果
//! （[`CardRecord`]）：均值、峰值、峰值时间。空序列返回 `None`（报表显示 N/A）。
//! HBM fallback 与归属取值的逻辑也落在这里（见后续函数）。

use chrono::{DateTime, Utc};

/// 一张卡的时间范围内统计结果——对应报表一行。
///
/// 所有数值字段为 `Option`：`None` 表示该卡/该指标无有效数据，报表显示 N/A。
#[derive(Debug, Clone, PartialEq)]
pub struct CardRecord {
    /// 数据来源（Prometheus 别名）。
    pub source_name: String,
    /// 主机 IP。
    pub host_ip: String,
    /// 节点名称（标签，可能为空字符串）。
    pub node_name: String,
    /// 计算卡编号。
    pub card_id: String,
    /// 设备类型显示名。
    pub device_type: String,
    /// Namespace 归属。
    pub namespace: String,
    /// Pod 归属。
    pub pod: String,
    /// 容器归属。
    pub container: String,
    /// 核心利用率平均值（0–100）。None = N/A。
    pub core_avg: Option<f64>,
    /// 核心利用率峰值。
    pub core_peak: Option<f64>,
    /// 核心峰值出现时间。
    pub core_peak_time: Option<DateTime<Utc>>,
    /// 显存占用率平均值。
    pub mem_avg: Option<f64>,
    /// 显存占用率峰值。
    pub mem_peak: Option<f64>,
    /// 显存峰值出现时间。
    pub mem_peak_time: Option<DateTime<Utc>>,
    /// 核心利用率数据点数量。None = N/A。
    pub core_count: Option<usize>,
    /// 核心利用率第一条数据时间。None = N/A。
    pub core_first_time: Option<DateTime<Utc>>,
    /// 核心利用率最后一条数据时间。None = N/A。
    pub core_last_time: Option<DateTime<Utc>>,
    /// 显存占用率数据点数量。None = N/A。
    pub mem_count: Option<usize>,
    /// 显存占用率第一条数据时间。None = N/A。
    pub mem_first_time: Option<DateTime<Utc>>,
    /// 显存占用率最后一条数据时间。None = N/A。
    pub mem_last_time: Option<DateTime<Utc>>,
    /// 设备温度平均值（°C）。None = N/A（设备类型未配置温度指标时）。
    pub temp_avg: Option<f64>,
    /// 设备温度峰值。
    pub temp_peak: Option<f64>,
    /// 温度峰值出现时间。
    pub temp_peak_time: Option<DateTime<Utc>>,
    /// 温度数据点数量。
    pub temp_count: Option<usize>,
    /// 温度首条数据时间。
    pub temp_first_time: Option<DateTime<Utc>>,
    /// 温度末条数据时间。
    pub temp_last_time: Option<DateTime<Utc>>,
    /// 设备功率平均值（W）。None = N/A（设备类型未配置功率指标时）。
    pub power_avg: Option<f64>,
    /// 设备功率峰值。
    pub power_peak: Option<f64>,
    /// 功率峰值出现时间。
    pub power_peak_time: Option<DateTime<Utc>>,
    /// 功率数据点数量。
    pub power_count: Option<usize>,
    /// 功率首条数据时间。
    pub power_first_time: Option<DateTime<Utc>>,
    /// 功率末条数据时间。
    pub power_last_time: Option<DateTime<Utc>>,
    /// 主机 CPU 利用率平均值（%）。None = N/A（未启用主机指标采集时）。
    pub host_cpu_avg: Option<f64>,
    /// 主机 CPU 利用率峰值。
    pub host_cpu_peak: Option<f64>,
    /// 主机 CPU 峰值出现时间。
    pub host_cpu_peak_time: Option<DateTime<Utc>>,
    /// 主机内存利用率平均值（%）。None = N/A。
    pub host_mem_avg: Option<f64>,
    /// 主机内存利用率峰值。
    pub host_mem_peak: Option<f64>,
    /// 主机内存峰值出现时间。
    pub host_mem_peak_time: Option<DateTime<Utc>>,
    /// 主机句柄数平均值。None = N/A。
    pub host_handle_avg: Option<f64>,
    /// 主机句柄数峰值。
    pub host_handle_peak: Option<f64>,
    /// 主机句柄数峰值出现时间。
    pub host_handle_peak_time: Option<DateTime<Utc>>,
    /// 取值时间范围起点。
    pub range_start: DateTime<Utc>,
    /// 取值时间范围终点。
    pub range_end: DateTime<Utc>,
}

/// 一个带标签的时序序列（由 fetcher 产出）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Series {
    /// Prometheus 标签集合。
    pub labels: std::collections::HashMap<String, String>,
    /// (时间戳, 数值) 时间序列点。
    pub points: Vec<(DateTime<Utc>, f64)>,
}

/// 单指标的聚合统计。
#[derive(Debug, Clone, PartialEq)]
pub struct MetricStats {
    /// 算术平均。
    pub avg: f64,
    /// 峰值。
    pub peak: f64,
    /// 峰值出现时间。
    pub peak_time: DateTime<Utc>,
    /// 参与计算的数据点数量。
    pub count: usize,
    /// 第一条数据的时间戳。
    pub first_time: DateTime<Utc>,
    /// 最后一条数据的时间戳。
    pub last_time: DateTime<Utc>,
}

/// 对一组点做均值/峰值/峰值时间聚合。
///
/// 空输入返回 `None`。峰值取最大值；多个点同为最大时取最早的时间戳（稳定）。
///
/// # Panics
///
/// 当 `points` 非空时内部使用 `unwrap()` 取 `min`/`max` 时间戳——非空迭代器保证安全。
#[must_use]
#[allow(clippy::missing_panics_doc)]
pub fn aggregate(points: &[(DateTime<Utc>, f64)]) -> Option<MetricStats> {
    if points.is_empty() {
        return None;
    }
    // 过滤非有限值（NaN/Inf），防止下游写入数据库时产生 "NaN"/"inf" 字符串
    let finite_points: Vec<(DateTime<Utc>, f64)> = points
        .iter()
        .filter(|(_, v)| v.is_finite())
        .copied()
        .collect();
    if finite_points.is_empty() {
        return None;
    }
    let count = finite_points.len();
    let sum: f64 = finite_points.iter().map(|(_, v)| *v).sum();
    #[allow(clippy::cast_precision_loss)]
    let avg = sum / count as f64;
    // 取最大值；并列取最早时间戳（va 相同时 tb 越小越优先 → .then(tb.cmp(ta))）。
    // max_by 对非空迭代器必返回 Some；这里用 match 显式处理，避免 expect/panic。
    let best = finite_points.iter().copied().max_by(|(ta, va), (tb, vb)| {
        va.partial_cmp(vb)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(tb.cmp(ta))
    });
    // 首/末数据时间：按时间戳排序取最早和最晚
    let first_time = finite_points.iter().map(|(ts, _)| *ts).min().unwrap(); // 非空迭代器，min 必返回 Some
    let last_time = finite_points.iter().map(|(ts, _)| *ts).max().unwrap();
    best.map(|(peak_time, peak)| MetricStats {
        avg,
        peak,
        peak_time,
        count,
        first_time,
        last_time,
    })
}

/// HBM fallback：当直接利用率指标为空时，用 used/total*100 重算显存占用率序列。
///
/// `used`/`total` 为显存字节/MB 的原始序列。返回 fallback 后的 [`Series`]：
/// 点数与 used 对齐（按 timestamp 与 total 对齐），total 为 0 的点丢弃。
/// 调用方应：先尝试 `aggregate(direct.points)`；为空时再调用本函数并聚合结果。
#[must_use]
pub fn hbm_fallback_series(used: &Series, total: &Series) -> Series {
    // 按 timestamp 对齐 used 与 total
    let total_map: std::collections::HashMap<i64, f64> = total
        .points
        .iter()
        .map(|(ts, v)| (ts.timestamp(), *v))
        .collect();
    let mut points = Vec::new();
    for (ts, u) in &used.points {
        if let Some(tot) = total_map.get(&ts.timestamp()) {
            if *tot > 0.0 {
                // 防御性 clamp：Prometheus used/total 计数器采集时差可能导致
                // used > total，产出超过 100% 的无效利用率。clamp 到 [0, 100]。
                let v = (u / tot * 100.0).clamp(0.0, 100.0);
                // 防御性过滤：除法可能产出 Inf（u 极大 / tot 极小）或 NaN，
                // 绕过 fetcher 的 parse-time NaN 过滤器，导致 aggregate 结果错误。
                if v.is_finite() {
                    points.push((*ts, v));
                }
            }
        }
    }
    Series {
        labels: used.labels.clone(),
        points,
    }
}

/// 从一组归属时序点中取"末态"标签值（最后一个非空字符串）。
///
/// `tagged_points` 是 (时间戳, 该标签值) 序列；空或全空返回空串。
/// 由 pipeline 的 `last_in_range` 归属模式调用（PRD §2.4）。
#[must_use]
pub fn last_non_empty(tagged_points: &[(DateTime<Utc>, String)]) -> String {
    tagged_points
        .iter()
        .rev()
        .find(|(_, v)| !v.is_empty())
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::collections::HashMap;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    #[test]
    fn aggregate_empty_returns_none() {
        assert!(aggregate(&[]).is_none());
    }

    #[test]
    fn aggregate_computes_avg_peak_peaktime() {
        let pts = vec![(t(0), 10.0), (t(60), 40.0), (t(120), 70.0)];
        let s = aggregate(&pts).unwrap();
        assert!((s.avg - 40.0).abs() < 1e-9);
        assert!((s.peak - 70.0).abs() < 1e-9);
        assert_eq!(s.peak_time, t(120));
    }

    #[test]
    fn aggregate_tie_picks_earliest_timestamp() {
        let pts = vec![(t(60), 50.0), (t(0), 50.0)];
        let s = aggregate(&pts).unwrap();
        assert_eq!(s.peak_time, t(0), "并列峰值应取最早时间戳");
    }

    #[test]
    fn hbm_fallback_divides_used_by_total() {
        let used = Series {
            labels: HashMap::default(),
            points: vec![(t(0), 50.0), (t(60), 60.0)],
        };
        let total = Series {
            labels: HashMap::default(),
            points: vec![(t(0), 200.0), (t(60), 0.0)], // t60 total=0 应被丢弃
        };
        let fb = hbm_fallback_series(&used, &total);
        assert_eq!(fb.points.len(), 1);
        assert!((fb.points[0].1 - 25.0).abs() < 1e-9); // 50/200*100
    }

    #[test]
    fn hbm_fallback_clamps_over_100() {
        // used > total（Prometheus 计数器采集时差），产出 >100% 应被 clamp 到 100。
        let used = Series {
            labels: HashMap::default(),
            points: vec![(t(0), 110.0)],
        };
        let total = Series {
            labels: HashMap::default(),
            points: vec![(t(0), 100.0)],
        };
        let fb = hbm_fallback_series(&used, &total);
        assert_eq!(fb.points.len(), 1);
        assert!((fb.points[0].1 - 100.0).abs() < 1e-9, "应被 clamp 到 100%");
    }

    #[test]
    fn hbm_fallback_clamps_inf_to_100() {
        // 极大 used / 极小 total → Inf，clamp 后为 100.0（满载）。
        let used = Series {
            labels: HashMap::default(),
            points: vec![(t(0), f64::MAX), (t(60), 50.0)],
        };
        let total = Series {
            labels: HashMap::default(),
            points: vec![(t(0), f64::MIN_POSITIVE), (t(60), 200.0)],
        };
        let fb = hbm_fallback_series(&used, &total);
        assert_eq!(fb.points.len(), 2, "Inf 应被 clamp 为 100.0 而非丢弃");
        assert!((fb.points[0].1 - 100.0).abs() < 1e-9, "Inf → clamp → 100%");
        assert!((fb.points[1].1 - 25.0).abs() < 1e-9);
    }

    #[test]
    fn last_non_empty_picks_latest_nonempty() {
        let pts = vec![
            (t(0), "pod-a".to_string()),
            (t(60), String::new()),
            (t(120), "pod-b".to_string()),
        ];
        assert_eq!(last_non_empty(&pts), "pod-b");
    }

    #[test]
    fn aggregate_all_nan_returns_none() {
        let pts = vec![(t(0), f64::NAN), (t(60), f64::NAN)];
        assert!(aggregate(&pts).is_none(), "全 NaN 输入应返回 None");
    }

    #[test]
    fn aggregate_all_inf_returns_none() {
        let pts = vec![(t(0), f64::INFINITY), (t(60), f64::NEG_INFINITY)];
        assert!(aggregate(&pts).is_none(), "全 Inf 输入应返回 None");
    }

    #[test]
    fn aggregate_filters_nan_keeps_finite() {
        let pts = vec![(t(0), 10.0), (t(60), f64::NAN), (t(120), 30.0)];
        let s = aggregate(&pts).unwrap();
        assert!((s.avg - 20.0).abs() < 1e-9, "应忽略 NaN，仅对有限值求均值");
        assert!((s.peak - 30.0).abs() < 1e-9);
        assert_eq!(s.count, 2);
    }

    #[test]
    fn aggregate_filters_inf_keeps_finite() {
        let pts = vec![(t(0), f64::INFINITY), (t(60), 50.0), (t(120), f64::NEG_INFINITY)];
        let s = aggregate(&pts).unwrap();
        assert!((s.avg - 50.0).abs() < 1e-9, "应忽略 Inf，仅对有限值求均值");
        assert!((s.peak - 50.0).abs() < 1e-9);
        assert_eq!(s.count, 1);
    }

    #[test]
    fn last_non_empty_all_empty_returns_empty() {
        let pts = vec![(t(0), String::new()), (t(60), String::new())];
        assert_eq!(last_non_empty(&pts), "");
    }
}
