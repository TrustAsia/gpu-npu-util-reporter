//! 数据源适配层模块。
//!
//! 通过 [`MetricFetcher`] trait 抽象"查询某 `PromQL` 在时间范围内的时序"，
//! 具体实现 [`PrometheusFetcher`] 走 HTTP `/api/v1/query_range` 与
//! `/api/v1/query`。fetcher 还负责把显存组合公式翻译成单条 `PromQL`
//! （[`gpu_memory_promql`]）。

use crate::error::AppError;
use crate::processor::Series;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

/// fetcher 查询的抽象接口，便于测试用 mock 替换真实 HTTP。
#[async_trait]
pub trait MetricFetcher: Send + Sync {
    /// range query：返回多条带标签的时序。
    async fn query_range(
        &self,
        promql: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        step: Duration,
    ) -> Result<Vec<Series>, AppError>;

    /// instant query：返回当前时刻的标签值集合。
    ///
    /// 用于 `ownership.mode = instant` 的归属瞬时查询（PRD §2.4）。
    async fn query_instant(&self, promql: &str) -> Result<Vec<Series>, AppError>;
}

/// 调用 Prometheus HTTP API 的实现。
pub struct PrometheusFetcher {
    client: reqwest::Client,
    base_url: String,
    timeout: std::time::Duration,
    /// 用于错误提示的数据源别名。
    source_name: String,
}

impl PrometheusFetcher {
    #[must_use]
    pub fn new(source_name: String, base_url: String, timeout_secs: u64) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            timeout: std::time::Duration::from_secs(timeout_secs),
            source_name,
        }
    }
}

/// Prometheus `/api/v1/query_range` 的 JSON 响应（仅取需要的字段）。
#[derive(serde::Deserialize)]
struct PromResponse {
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    data: Option<PromData>,
}

#[derive(serde::Deserialize, Default)]
struct PromData {
    #[serde(default)]
    result: Vec<PromResult>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum PromResult {
    // Vector 放在前面：serde untagged 按声明顺序尝试，Vector 更具体（单值 vs 数组），
    // 优先匹配可避免 Vector 被误解析为单元素 Matrix。
    Vector {
        metric: HashMap<String, String>,
        value: (f64, String),
    },
    Matrix {
        metric: HashMap<String, String>,
        values: Vec<(f64, String)>, // (unix_ts, value 字符串)
    },
}

#[async_trait]
impl MetricFetcher for PrometheusFetcher {
    async fn query_range(
        &self,
        promql: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        step: Duration,
    ) -> Result<Vec<Series>, AppError> {
        let url = format!("{}/api/v1/query_range", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .get(&url)
            .query(&[
                ("query", promql),
                ("start", &start.timestamp().to_string()),
                ("end", &end.timestamp().to_string()),
                ("step", &step.num_seconds().to_string()),
            ])
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| AppError::Prometheus {
                source_name: self.source_name.clone(),
                url: self.base_url.clone(),
                detail: format!("连接失败：{e}"),
            })?;
        let body: PromResponse = resp.json().await.map_err(|e| AppError::Prometheus {
            source_name: self.source_name.clone(),
            url: self.base_url.clone(),
            detail: format!("解析响应失败：{e}"),
        })?;
        parse_response(body, &self.source_name)
    }

    async fn query_instant(&self, promql: &str) -> Result<Vec<Series>, AppError> {
        let url = format!("{}/api/v1/query", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .get(&url)
            .query(&[("query", promql)])
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| AppError::Prometheus {
                source_name: self.source_name.clone(),
                url: self.base_url.clone(),
                detail: format!("连接失败：{e}"),
            })?;
        let body: PromResponse = resp.json().await.map_err(|e| AppError::Prometheus {
            source_name: self.source_name.clone(),
            url: self.base_url.clone(),
            detail: format!("解析响应失败：{e}"),
        })?;
        parse_response(body, &self.source_name)
    }
}

/// 把 Prometheus 响应转成 Series 列表。Vector 形式当作单点序列。
fn parse_response(resp: PromResponse, source: &str) -> Result<Vec<Series>, AppError> {
    if resp.status != "success" {
        return Err(AppError::Promql {
            source_name: source.into(),
            detail: resp.error.unwrap_or_else(|| "未知错误".into()),
        });
    }
    let data = resp.data.unwrap_or_default();
    let mut out = Vec::new();
    for r in data.result {
        match r {
            PromResult::Matrix { metric, values } => {
                let mut points = Vec::with_capacity(values.len());
                for (ts, val) in values {
                    if let Ok(v) = val.parse::<f64>() {
                        // I4 修复：丢弃 NaN/Inf。Prometheus 可能返回 NaN（如除零
                        // 指标 FB_USED/(FB_USED+0)），不过滤会让 aggregate 的均值
                        // 变成 NaN、峰值排序行为未定义。
                        if !v.is_finite() {
                            continue;
                        }
                        // Prometheus 时间戳为 Unix 秒（f64），转 i64 截断是安全的：
                        // 实际值范围远在 i64 精度内。
                        #[allow(clippy::cast_possible_truncation)]
                        if let Some(dt) = DateTime::<Utc>::from_timestamp(ts as i64, 0) {
                            points.push((dt, v));
                        }
                    }
                }
                out.push(Series {
                    labels: metric,
                    points,
                });
            }
            PromResult::Vector { metric, value } => {
                if let Ok(v) = value.1.parse::<f64>() {
                    if v.is_finite() {
                        #[allow(clippy::cast_possible_truncation)]
                        if let Some(dt) = DateTime::<Utc>::from_timestamp(value.0 as i64, 0) {
                            out.push(Series {
                                labels: metric,
                                points: vec![(dt, v)],
                            });
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// 由 GPU 显存策略生成单条 `PromQL`（`FB_USED/(FB_USED+FB_FREE)*100`）。
#[must_use]
pub fn gpu_memory_promql(used: &str, free: &str) -> String {
    format!("{used} / ({used} + {free}) * 100")
}

/// 测试用 Mock fetcher：按 promql 子串匹配返回预设序列，或注入错误。
///
/// 设计：`responses` 是 (子串谓词, 返回值) 列表，按声明顺序首次命中即返回；
/// 都不命中则返回空 `Ok`。这样编排测试可以针对不同指标（核心 vs 显存 vs
/// fallback 的 used/total）返回不同序列，或对某条查询返回 `Err` 来验证
/// C2（fetch 失败应转 Warning 而非静默 N/A）。
#[cfg(test)]
pub struct MockFetcher {
    /// (promql 子串谓词, 返回结果)
    pub responses: Vec<(String, Result<Vec<Series>, AppError>)>,
}

#[cfg(test)]
impl Default for MockFetcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl MockFetcher {
    #[must_use]
    pub fn new() -> Self {
        Self {
            responses: Vec::new(),
        }
    }
    /// 注册：当 promql 含 `needle` 子串时返回 `res`。
    #[must_use]
    pub fn when(mut self, needle: impl Into<String>, res: Result<Vec<Series>, AppError>) -> Self {
        self.responses.push((needle.into(), res));
        self
    }
    fn lookup(&self, promql: &str) -> Result<Vec<Series>, AppError> {
        for (needle, res) in &self.responses {
            if promql.contains(needle.as_str()) {
                return res.clone();
            }
        }
        Ok(Vec::new())
    }
}

#[cfg(test)]
#[async_trait]
impl MetricFetcher for MockFetcher {
    async fn query_range(
        &self,
        promql: &str,
        _start: DateTime<Utc>,
        _end: DateTime<Utc>,
        _step: Duration,
    ) -> Result<Vec<Series>, AppError> {
        self.lookup(promql)
    }
    async fn query_instant(&self, promql: &str) -> Result<Vec<Series>, AppError> {
        self.lookup(promql)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_matrix_response() {
        let resp = PromResponse {
            status: "success".into(),
            error: None,
            data: Some(PromData {
                result: vec![PromResult::Matrix {
                    metric: HashMap::from([("gpu".into(), "0".into())]),
                    values: vec![(1000.0, "50.0".into()), (1060.0, "75.0".into())],
                }],
            }),
        };
        let s = parse_response(resp, "src").unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].points.len(), 2);
        assert!((s[0].points[1].1 - 75.0).abs() < 1e-9);
    }

    #[test]
    fn parse_error_status() {
        let resp = PromResponse {
            status: "error".into(),
            error: Some("bad_data".into()),
            data: None,
        };
        assert!(parse_response(resp, "src").is_err());
    }

    #[test]
    fn gpu_memory_promql_format() {
        let q = gpu_memory_promql("DCGM_FI_DEV_FB_USED", "DCGM_FI_DEV_FB_FREE");
        assert!(
            q.contains("DCGM_FI_DEV_FB_USED / (DCGM_FI_DEV_FB_USED + DCGM_FI_DEV_FB_FREE) * 100")
        );
    }

    #[test]
    fn parse_response_drops_nan_and_inf() {
        // I4：Prometheus 可能返回 "NaN"/"+Inf"/"-Inf"（如除零指标），应被丢弃，
        // 否则 aggregate 的均值会变 NaN。
        let resp = PromResponse {
            status: "success".into(),
            error: None,
            data: Some(PromData {
                result: vec![PromResult::Matrix {
                    metric: HashMap::from([("gpu".into(), "0".into())]),
                    values: vec![
                        (1000.0, "50.0".into()),
                        (1060.0, "NaN".into()),
                        (1120.0, "+Inf".into()),
                        (1180.0, "75.0".into()),
                    ],
                }],
            }),
        };
        let s = parse_response(resp, "src").unwrap();
        assert_eq!(s[0].points.len(), 2, "NaN/+Inf 应被过滤");
        assert!((s[0].points[0].1 - 50.0).abs() < 1e-9);
        assert!((s[0].points[1].1 - 75.0).abs() < 1e-9);
    }
}
