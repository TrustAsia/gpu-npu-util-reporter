//! 数据源适配层模块。
//!
//! 通过 [`MetricFetcher`] trait 抽象"查询某 PromQL 在时间范围内的时序"，
//! 具体实现 [`PrometheusFetcher`] 走 HTTP `/api/v1/query_range` 与
//! `/api/v1/query`。fetcher 还负责把显存组合公式翻译成单条 PromQL
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
    Matrix {
        metric: HashMap<String, String>,
        values: Vec<(f64, String)>, // (unix_ts, value 字符串)
    },
    Vector {
        metric: HashMap<String, String>,
        value: (f64, String),
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
        let url = format!(
            "{}/api/v1/query_range",
            self.base_url.trim_end_matches('/')
        );
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
        let body: PromResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Prometheus {
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
        let body: PromResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Prometheus {
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
    Ok(out)
}

/// 由 GPU 显存策略生成单条 PromQL（FB_USED/(FB_USED+FB_FREE)*100）。
pub fn gpu_memory_promql(used: &str, free: &str) -> String {
    format!("{used} / ({used} + {free}) * 100")
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
        assert!(q.contains("DCGM_FI_DEV_FB_USED / (DCGM_FI_DEV_FB_USED + DCGM_FI_DEV_FB_FREE) * 100"));
    }

    /// Mock fetcher：返回预设序列，用于编排逻辑测试（不连真实 Prometheus）。
    pub struct MockFetcher {
        pub series: Vec<Series>,
    }

    #[async_trait]
    impl MetricFetcher for MockFetcher {
        async fn query_range(
            &self,
            _promql: &str,
            _start: DateTime<Utc>,
            _end: DateTime<Utc>,
            _step: Duration,
        ) -> Result<Vec<Series>, AppError> {
            Ok(self.series.clone())
        }
        async fn query_instant(&self, _promql: &str) -> Result<Vec<Series>, AppError> {
            Ok(self.series.clone())
        }
    }
}
