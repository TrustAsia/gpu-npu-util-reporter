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
}

/// 对一组点做均值/峰值/峰值时间聚合。
///
/// 空输入返回 `None`。峰值取最大值；多个点同为最大时取最早的时间戳（稳定）。
pub fn aggregate(points: &[(DateTime<Utc>, f64)]) -> Option<MetricStats> {
    if points.is_empty() {
        return None;
    }
    let sum: f64 = points.iter().map(|(_, v)| *v).sum();
    let avg = sum / points.len() as f64;
    // 取最大值；并列取最早时间戳（va 相同时 tb 越小越优先 → .then(tb.cmp(ta))）
    let (peak_time, peak) = points
        .iter()
        .copied()
        .max_by(|(ta, va), (tb, vb)| {
            va.partial_cmp(vb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(tb.cmp(ta))
        })
        .expect("非空序列必有最大值");
    Some(MetricStats {
        avg,
        peak,
        peak_time,
    })
}

/// HBM fallback：当直接利用率指标为空时，用 used/total*100 重算显存占用率序列。
///
/// `used`/`total` 为显存字节/MB 的原始序列。返回 fallback 后的 [`Series`]：
/// 点数与 used 对齐（按 timestamp 与 total 对齐），total 为 0 的点丢弃。
/// 调用方应：先尝试 `aggregate(direct.points)`；为空时再调用本函数并聚合结果。
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
                points.push((*ts, u / tot * 100.0));
            }
        }
    }
    Series {
        labels: used.labels.clone(),
        points,
    }
}

/// 归属取值模式（PRD §2.4）。
///
/// 预留给完整的 last_in_range 归属实现；当前 main 用 range 查询的标签瞬时值
/// 近似归属，故枚举与 `last_non_empty` 暂未在编排中调用。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipMode {
    /// 瞬时值：查询时刻的标签。
    Instant,
    /// 末态值：时间范围内最后一个非空标签。
    LastInRange,
}

/// 从一组归属时序点中取"末态"标签值（最后一个非空字符串）。
///
/// `tagged_points` 是 (时间戳, 该标签值) 序列；空或全空返回空串。
/// 预留给完整的 last_in_range 归属实现。
#[allow(dead_code)]
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
            labels: Default::default(),
            points: vec![(t(0), 50.0), (t(60), 60.0)],
        };
        let total = Series {
            labels: Default::default(),
            points: vec![(t(0), 200.0), (t(60), 0.0)], // t60 total=0 应被丢弃
        };
        let fb = hbm_fallback_series(&used, &total);
        assert_eq!(fb.points.len(), 1);
        assert!((fb.points[0].1 - 25.0).abs() < 1e-9); // 50/200*100
    }

    #[test]
    fn last_non_empty_picks_latest_nonempty() {
        let pts = vec![
            (t(0), "pod-a".to_string()),
            (t(60), "".to_string()),
            (t(120), "pod-b".to_string()),
        ];
        assert_eq!(last_non_empty(&pts), "pod-b");
    }

    #[test]
    fn last_non_empty_all_empty_returns_empty() {
        let pts = vec![(t(0), "".to_string()), (t(60), "".to_string())];
        assert_eq!(last_non_empty(&pts), "");
    }
}
