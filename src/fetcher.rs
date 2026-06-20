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

/// Prometheus 响应体大小上限（100 MB），防止异常大响应耗尽内存。
const MAX_RESPONSE_BYTES: u64 = 100 * 1024 * 1024;

impl PrometheusFetcher {
    #[must_use]
    pub fn new(source_name: String, base_url: String, timeout_secs: u64) -> Self {
        Self {
            client: reqwest::ClientBuilder::new()
                .timeout(std::time::Duration::from_secs(timeout_secs))
                .build()
                .unwrap_or_default(),
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
                ("start", &start.to_rfc3339()),
                ("end", &end.to_rfc3339()),
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
        let resp = resp.error_for_status().map_err(|e| AppError::Prometheus {
            source_name: self.source_name.clone(),
            url: self.base_url.clone(),
            detail: format!("HTTP 请求失败：{e}"),
        })?;
        let data: PromResponse =
            read_limited_json(resp, &self.source_name, &self.base_url).await?;
        parse_response(data, &self.source_name)
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
        let resp = resp.error_for_status().map_err(|e| AppError::Prometheus {
            source_name: self.source_name.clone(),
            url: self.base_url.clone(),
            detail: format!("HTTP 请求失败：{e}"),
        })?;
        let data: PromResponse =
            read_limited_json(resp, &self.source_name, &self.base_url).await?;
        parse_response(data, &self.source_name)
    }
}

/// 检查响应体大小限制（基于 Content-Length 头的快速拒绝）。
///
/// 实际大小校验在 [`read_limited_json`] 中读取字节后完成，
/// 防止 chunked 传输绕过 Content-Length 检查。
fn check_response_size_header(
    resp: &reqwest::Response,
    source_name: &str,
    url: &str,
) -> Result<(), AppError> {
    if let Some(len) = resp.content_length() {
        if len > MAX_RESPONSE_BYTES {
            return Err(AppError::Prometheus {
                source_name: source_name.into(),
                url: url.into(),
                detail: format!("响应体过大（{len} 字节，上限 {MAX_RESPONSE_BYTES} 字节）"),
            });
        }
    }
    Ok(())
}

/// 读取响应体字节，校验实际大小（防止 chunked 传输绕过 Content-Length），
/// 然后反序列化为指定类型。
async fn read_limited_json<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
    source_name: &str,
    url: &str,
) -> Result<T, AppError> {
    check_response_size_header(&resp, source_name, url)?;
    let body = resp.bytes().await.map_err(|e| AppError::Prometheus {
        source_name: source_name.into(),
        url: url.into(),
        detail: format!("读取响应体失败：{e}"),
    })?;
    // 实际大小校验：chunked 传输无 Content-Length，必须读完后检查。
    #[allow(clippy::cast_possible_truncation)]
    if body.len() > MAX_RESPONSE_BYTES as usize {
        return Err(AppError::Prometheus {
            source_name: source_name.into(),
            url: url.into(),
            detail: format!(
                "响应体过大（{} 字节，上限 {MAX_RESPONSE_BYTES} 字节）",
                body.len()
            ),
        });
    }
    serde_json::from_slice(&body).map_err(|e| AppError::Prometheus {
        source_name: source_name.into(),
        url: url.into(),
        detail: format!("解析响应失败：{e}"),
    })
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
                if !points.is_empty() {
                    out.push(Series {
                        labels: metric,
                        points,
                    });
                }
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
///
/// `ignoring(__name__)` 必不可少：Prometheus 二元运算在两个即时向量间按所有标签
/// （含 `__name__`）匹配。`used` 与 `free` 的 `__name__` 不同，不加 `ignoring`
/// 会导致内层加法返回空集，进而整条表达式产出 0 条时序——所有 GPU 显存数据静默丢失。
#[must_use]
pub fn gpu_memory_promql(used: &str, free: &str) -> String {
    format!("{used} / ignoring(__name__) ({used} + ignoring(__name__) {free}) * 100")
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
            q.contains("ignoring(__name__)")
                && q.contains("DCGM_FI_DEV_FB_USED / ignoring(__name__)")
                && q.contains("DCGM_FI_DEV_FB_USED + ignoring(__name__) DCGM_FI_DEV_FB_FREE")
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

    #[test]
    fn parse_matrix_all_nan_produces_no_series() {
        // 当 Matrix 所有值均为 NaN/Inf 时，不应产生空 points 的 Series，
        // 否则下游 is_empty() 判断会认为有数据而阻断 fallback。
        let resp = PromResponse {
            status: "success".into(),
            error: None,
            data: Some(PromData {
                result: vec![PromResult::Matrix {
                    metric: HashMap::from([("gpu".into(), "0".into())]),
                    values: vec![
                        (1000.0, "NaN".into()),
                        (1060.0, "+Inf".into()),
                        (1120.0, "-Inf".into()),
                    ],
                }],
            }),
        };
        let s = parse_response(resp, "src").unwrap();
        assert!(s.is_empty(), "全 NaN/Inf 的 Matrix 不应产生空 Series");
    }

    #[test]
    fn parse_vector_response() {
        // Vector 形式（instant query）当作单点序列
        let resp = PromResponse {
            status: "success".into(),
            error: None,
            data: Some(PromData {
                result: vec![PromResult::Vector {
                    metric: HashMap::from([("gpu".into(), "0".into())]),
                    value: (1000.0, "50.0".into()),
                }],
            }),
        };
        let s = parse_response(resp, "src").unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].points.len(), 1);
        assert!((s[0].points[0].1 - 50.0).abs() < 1e-9);
        assert_eq!(s[0].labels["gpu"], "0");
    }

    #[test]
    fn parse_vector_drops_nan_and_inf() {
        // Vector 形式的 NaN/Inf 也应被过滤
        let resp = PromResponse {
            status: "success".into(),
            error: None,
            data: Some(PromData {
                result: vec![
                    PromResult::Vector {
                        metric: HashMap::from([("gpu".into(), "0".into())]),
                        value: (1000.0, "NaN".into()),
                    },
                    PromResult::Vector {
                        metric: HashMap::from([("gpu".into(), "1".into())]),
                        value: (1060.0, "50.0".into()),
                    },
                ],
            }),
        };
        let s = parse_response(resp, "src").unwrap();
        assert_eq!(s.len(), 1, "NaN 的 Vector 应被过滤");
        assert_eq!(s[0].labels["gpu"], "1");
    }
}
