//! 采集编排流水线模块（从 main.rs 抽出，提升可测试性）。
//!
//! 职责：给定一个 [`MetricFetcher`] + 设备配方 + 时间范围，拉取核心/显存时序，
//! 按 (`host_ip`, `card_id`) 分组聚合为 [`CardRecord`] 列表，并收集过程中的非致命
//! Warning（PRD §5.2：单源/单卡失败降级为 N/A 且记 Warning，而非静默吞掉）。
//!
//! 本模块是 Critical 修复的核心落点：
//! - **C2**：fetch 失败 → Warning（见 [`CollectOutcome`] 的 `warnings`）。
//! - **C3**：HBM fallback 不再依赖全 label 相等（含 `__name__`），改按
//!   (`host_ip`, `card_id`) join。
//! - **C1**：归属 `last_in_range` 模式：对每张卡额外查归属标签序列，
//!   取时间范围内最后一个非空值；`instant` 模式取查询返回的瞬时标签。
//! - **I7**：分组用 `(Option<Series>, Option<Series>)`，避免或无 core 数据时
//!   被静默覆写成空 series 而产生"幽灵行"。

use crate::config::AppConfig;
use crate::devices::{DeviceSpec, MemoryStrategy};
use crate::fetcher::MetricFetcher;
use crate::processor::{self, aggregate, last_non_empty, CardRecord, Series};
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

/// 采集过程中的共享上下文：时间范围、设备配方、配置、结果收集器。
///
/// 把 `fallback_used_total` / `ownership_for` 等函数的公共参数打包，
/// 避免函数签名过长（clippy `too_many_arguments`）。
struct QueryContext<'a> {
    fetcher: &'a dyn MetricFetcher,
    spec: &'a DeviceSpec,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step: Duration,
    out: &'a mut CollectOutcome,
}

/// 归属取值模式（PRD §2.4）。
///
/// `Instant`：直接取 range 查询返回的标签瞬时值。
/// `LastInRange`：对每张卡额外查归属标签序列，取时间范围内最后一个非空值——
/// 即使 Pod 在窗口中途漂移，也能锁定窗口末态归属。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipMode {
    Instant,
    LastInRange,
}

impl OwnershipMode {
    /// 从配置字符串解析；未知值回退到 `LastInRange`（与默认配置一致）。
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "instant" => Self::Instant,
            _ => Self::LastInRange,
        }
    }
}

/// 采集一个设备类型在一个源上的所有卡的结果。
#[derive(Debug, Default)]
pub struct CollectOutcome {
    /// 成功聚合的卡记录（按分组 key 升序稳定排序）。
    pub records: Vec<CardRecord>,
    /// 非致命 Warning（已含中文上下文，可直接打印）。
    pub warnings: Vec<String>,
}

impl CollectOutcome {
    fn push_warning(&mut self, msg: String) {
        self.warnings.push(msg);
    }
}

/// 采集一个设备类型在一个源上的所有卡。
///
/// 流程：
/// 1. 查核心利用率序列（失败→Warning，继续）。
/// 2. 按显存策略查显存序列（CompositeRatio / DirectMetric）；DirectMetric 为空
///    时触发 HBM fallback（拉 used/total，按 (`ip`, `card_id`) join 重算）。
/// 3. 按 (`host_ip`, `card_id`) 分组；同一卡的核心/显存各自占 `Option` 槽位，
///    互不覆写。只有 core 或只有 mem 的卡也会被保留（对应字段 N/A）。
/// 4. 归属：Instant 取标签瞬时值；LastInRange 额外查 namespace/pod/container
///    三条归属标签序列并取末态非空值。
#[allow(clippy::too_many_lines)]
pub async fn collect_device(
    fetcher: &dyn MetricFetcher,
    source_name: &str,
    spec: &DeviceSpec,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step: Duration,
    cfg: &AppConfig,
) -> CollectOutcome {
    let mut out = CollectOutcome::default();
    let mode = OwnershipMode::parse(&cfg.ownership.mode);
    let mut ctx = QueryContext {
        fetcher,
        spec,
        start,
        end,
        step,
        out: &mut out,
    };

    // 1. 核心利用率
    let core_series = match ctx
        .fetcher
        .query_range(&ctx.spec.core_util_metric, ctx.start, ctx.end, ctx.step)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            ctx.out.push_warning(format!("{e}"));
            Vec::new()
        }
    };

    // 2. 显存
    let mem_series = match &ctx.spec.memory {
        MemoryStrategy::CompositeRatio(b) => {
            let q =
                crate::fetcher::gpu_memory_promql(&b.composite_ratio.used, &b.composite_ratio.free);
            fetch_with_warning(&mut ctx, &q).await
        }
        MemoryStrategy::DirectMetric(b) => {
            fetch_with_warning(&mut ctx, &b.direct_metric.metric).await
        }
        MemoryStrategy::CompositeFromTotal(_) => {
            // 顶层 CompositeFromTotal：无 direct 指标，直接返空，由下方第 3 步的
            // CompositeFromTotal 分支按 used/total 计算（I8 修复：不再静默 N/A）。
            Vec::new()
        }
    };

    // 3. NPU fallback：direct/顶层显存为空时，按 used/total 重算（C3 + I8 修复）。
    //    - DirectMetric：direct 为空 → 用其 fallback（若有）。
    //    - CompositeFromTotal（顶层）：直接按 used/total 计算。
    let effective_mem = if mem_series.is_empty() {
        match &ctx.spec.memory {
            MemoryStrategy::DirectMetric(b) => {
                if let Some(fb) = &b.direct_metric.fallback {
                    fallback_used_total(&mut ctx, fb.as_ref()).await
                } else {
                    Vec::new()
                }
            }
            MemoryStrategy::CompositeFromTotal(top) => {
                // I8：顶层 used/total 直接计算，与 fallback 路径复用同一 join 逻辑。
                fallback_used_total(&mut ctx, &MemoryStrategy::CompositeFromTotal(top.clone()))
                    .await
            }
            MemoryStrategy::CompositeRatio(_) => Vec::new(),
        }
    } else {
        mem_series
    };

    // 4. 分组：(host_ip, card_id) → (core?, mem?)。同一卡多 series（如 Pod 漂移
    //    产出多条带不同归属标签的 series）的点会被合并进同一槽位，而非互相覆写，
    //    避免聚合数据丢失。槽位用 Option 互不覆写（I7 修复）。
    let mut groups: HashMap<String, (Option<Series>, Option<Series>)> = HashMap::new();
    for s in core_series {
        let key = series_key(&s, ctx.spec);
        merge_into(&mut groups.entry(key).or_default().0, s);
    }
    for s in effective_mem {
        let key = series_key(&s, ctx.spec);
        merge_into(&mut groups.entry(key).or_default().1, s);
    }

    // 稳定排序：按 key 升序输出，避免 HashMap 随机序。
    let mut keys: Vec<String> = groups.keys().cloned().collect();
    keys.sort();

    for key in keys {
        let (core, mem) = groups.remove(&key).unwrap_or((None, None));
        // I7：core 与 mem 都为 None 不应发生（至少有一路数据才会进入 groups）；
        // 防御性跳过避免幽灵行。
        if core.is_none() && mem.is_none() {
            continue;
        }

        // 身份字段优先从 core 取，core 缺失则从 mem 取。
        let id_series = core.as_ref().or(mem.as_ref());
        let host_ip = id_series
            .map(|s| extract_ip(&s.labels, &ctx.spec.labels.host_ip))
            .unwrap_or_default();
        let card_id = id_series
            .and_then(|s| s.labels.get(&ctx.spec.card_id_label).cloned())
            .unwrap_or_default();
        let node_name = id_series
            .and_then(|s| s.labels.get(&ctx.spec.labels.node_name).cloned())
            .unwrap_or_default();

        let (c_avg, c_peak, c_peak_t) = core
            .as_ref()
            .map_or((None, None, None), |c| stat3(&c.points));
        let (m_avg, m_peak, m_peak_t) = mem
            .as_ref()
            .map_or((None, None, None), |m| stat3(&m.points));

        // 归属
        let (namespace, pod, container) =
            ownership_for(&mut ctx, mode, &host_ip, &card_id, core.as_ref(), mem.as_ref()).await;

        ctx.out.records.push(CardRecord {
            source_name: source_name.into(),
            host_ip,
            node_name,
            card_id,
            device_type: ctx.spec.display_name.clone(),
            namespace,
            pod,
            container,
            core_avg: c_avg,
            core_peak: c_peak,
            core_peak_time: c_peak_t,
            mem_avg: m_avg,
            mem_peak: m_peak,
            mem_peak_time: m_peak_t,
            range_start: ctx.start,
            range_end: ctx.end,
        });
    }
    out
}

/// 把一条 series 合并进 `Option<Series>` 槽位：空则初始化；非空则把新点追加
/// 到既有 series 的 points 末尾，并按时间戳排序去重，保证聚合结果正确。
/// 保留首个 series 的标签（身份标签已由 join key 对齐，归属标签的取值由
/// `ownership_for` 单独处理）。
///
/// 这避免了 Pod 漂移场景下多条 series 共享同一 (`ip`, `card_id`) 时后者覆写前者、
/// 导致聚合丢点的问题。
fn merge_into(slot: &mut Option<Series>, incoming: Series) {
    match slot {
        Some(existing) => {
            existing.points.extend(incoming.points);
            existing.points.sort_by_key(|(ts, _)| *ts);
            // 同一时间戳保留最后一个值（最新观测），丢弃更早的点。
            // dedup_by 保留首个元素，因此先反转，去重后再反转回来。
            existing.points.reverse();
            existing.points.dedup_by(|a, b| a.0 == b.0);
            existing.points.reverse();
        }
        None => *slot = Some(incoming),
    }
}

/// 查询并吞错为 Warning（C2 修复：不再静默 `.unwrap_or_default()`）。
async fn fetch_with_warning(ctx: &mut QueryContext<'_>, promql: &str) -> Vec<Series> {
    match ctx
        .fetcher
        .query_range(promql, ctx.start, ctx.end, ctx.step)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            ctx.out.push_warning(format!("{e}"));
            Vec::new()
        }
    }
}

/// HBM fallback：拉 used/total，按 (`host_ip`, `card_id`) 对齐重算（C3 修复）。
///
/// 关键：不再用 `t.labels == u.labels` 比较——Prometheus matrix 结果带 `__name__`
/// 标签（used 与 total 的 `__name__` 不同），全 label 相等永远不成立，会导致
/// fallback 静默产出 0 点。改为按设备 join key 对齐。
async fn fallback_used_total(ctx: &mut QueryContext<'_>, fallback: &MemoryStrategy) -> Vec<Series> {
    let (used_metric, total_metric) = match fallback {
        MemoryStrategy::CompositeFromTotal(body) => (
            body.composite_from_total.used.as_str(),
            body.composite_from_total.total.as_str(),
        ),
        _ => return Vec::new(),
    };
    let used_s = fetch_with_warning(ctx, used_metric).await;
    let total_s = fetch_with_warning(ctx, total_metric).await;

    // total 按 join key 索引，避免 O(n*m) 全表扫且避开 `__name__` 干扰。
    // 同一 key 的多条 total series 需要合并（如 Pod 漂移产出不同标签的 series），
    // 否则 HashMap::collect 会静默丢弃非末尾的 series，导致数据丢失。
    let mut total_by_key: HashMap<String, Option<Series>> = HashMap::new();
    for t in total_s {
        let key = series_key(&t, ctx.spec);
        merge_into(total_by_key.entry(key).or_default(), t);
    }

    used_s
        .into_iter()
        .filter_map(|u| {
            let key = series_key(&u, ctx.spec);
            total_by_key
                .get(&key)
                .and_then(|opt| opt.as_ref())
                .map(|t| processor::hbm_fallback_series(&u, t))
        })
        .collect()
}

/// 解析单卡的归属（namespace/pod/container）。
///
/// `Instant`：直接从 range 查询返回的标签取（与原行为一致，向后兼容）。
/// `LastInRange`：重新查询该卡的核心利用率序列（按 `card_id_label` + `host_ip`
/// 标签过滤），收集所有点的归属标签值，按时间排序后取末态非空值——即使 Pod 在
/// 窗口中途漂移，也能锁定窗口内最后一个归属（PRD §2.4）。
///
/// `host_ip` 过滤确保多主机集群中只查目标主机的数据，避免跨主机归属错乱。
///
/// 查询失败按 Warning 降级，归属字段留空而非中断。
async fn ownership_for(
    ctx: &mut QueryContext<'_>,
    mode: OwnershipMode,
    host_ip: &str,
    card_id: &str,
    core: Option<&Series>,
    mem: Option<&Series>,
) -> (String, String, String) {
    if mode == OwnershipMode::Instant {
        // 先尝试从 core 取归属标签；若 core 缺失或归属字段为空，则回退到 mem。
        let core_labels = core.map(|c| &c.labels);
        let mem_labels = mem.map(|m| &m.labels);
        // 逐字段回退：core 有值用 core，否则从 mem 取。
        let get = |k: &str| -> String {
            if let Some(cl) = core_labels {
                if let Some(v) = cl.get(k) {
                    if !v.is_empty() {
                        return v.clone();
                    }
                }
            }
            mem_labels
                .and_then(|m| m.get(k))
                .cloned()
                .unwrap_or_default()
        };
        return (
            get(&ctx.spec.labels.namespace),
            get(&ctx.spec.labels.pod),
            get(&ctx.spec.labels.container),
        );
    }

    // LastInRange：按 card_id + host_ip 过滤重新拉取该卡的归属时序。Pod 漂移会产出
    // 多条 series（每条带不同归属标签集），按点的最大时间戳排序取末态非空。
    // host_ip 过滤确保多主机集群中只查目标主机，避免跨主机归属错乱。
    // 对 card_id/host_ip 值做 PromQL 转义，防止标签值中的引号/反斜杠/换行破坏查询语法。
    let escaped = card_id
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    let ip_escaped = host_ip
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    let promql = format!(
        "{metric}{{{a}=\"{v}\",{ip_label}=\"{ip}\"}}",
        metric = ctx.spec.core_util_metric,
        a = ctx.spec.card_id_label,
        v = escaped,
        ip_label = ctx.spec.labels.host_ip,
        ip = ip_escaped
    );
    match ctx
        .fetcher
        .query_range(&promql, ctx.start, ctx.end, ctx.step)
        .await
    {
        Ok(series) if !series.is_empty() => {
            let ns = last_label_value(&series, &ctx.spec.labels.namespace);
            let pod = last_label_value(&series, &ctx.spec.labels.pod);
            let ct = last_label_value(&series, &ctx.spec.labels.container);
            (ns, pod, ct)
        }
        Ok(_) => {
            // 核心指标无数据（如该卡只有显存数据），回退到显存指标查询归属。
            // 依次尝试所有显存指标（含 fallback 链），因为 DirectMetric 的主指标
            // 无数据时，fallback 的 used 指标仍可能有归属标签。
            let mem_names = ctx.spec.memory.ownership_metric_names();
            let mut ownership = (String::new(), String::new(), String::new());
            for mem_metric in &mem_names {
                let mem_promql = format!(
                    "{metric}{{{a}=\"{v}\",{ip_label}=\"{ip}\"}}",
                    metric = mem_metric,
                    a = ctx.spec.card_id_label,
                    v = escaped,
                    ip_label = ctx.spec.labels.host_ip,
                    ip = ip_escaped
                );
                match ctx
                    .fetcher
                    .query_range(&mem_promql, ctx.start, ctx.end, ctx.step)
                    .await
                {
                    Ok(mem_series) if !mem_series.is_empty() => {
                        let ns = last_label_value(&mem_series, &ctx.spec.labels.namespace);
                        let pod = last_label_value(&mem_series, &ctx.spec.labels.pod);
                        let ct = last_label_value(&mem_series, &ctx.spec.labels.container);
                        ownership = (ns, pod, ct);
                        break;
                    }
                    _ => {}
                }
            }
            ownership
        }
        Err(e) => {
            ctx.out.push_warning(format!("{e}"));
            (String::new(), String::new(), String::new())
        }
    }
}

/// 从多条 series 中取某标签的末态非空值：把每条 series 的 (最大时间戳, 标签值)
/// 收集后按时间排序，取末态非空（PRD §2.4 `last_in_range` 语义）。
///
/// 每条 series 用其最大点时间戳代表"该标签值出现的最晚时刻"——Pod 漂移产出
/// 不同 series，最晚出现的 series 即窗口末态归属。
fn last_label_value(series: &[Series], label: &str) -> String {
    let mut tagged: Vec<(DateTime<Utc>, String)> = series
        .iter()
        .filter_map(|s| {
            let v = s.labels.get(label)?;
            let max_ts = s.points.iter().map(|(ts, _)| *ts).max()?;
            Some((max_ts, v.clone()))
        })
        .collect();
    tagged.sort_by_key(|(ts, _)| *ts);
    last_non_empty(&tagged)
}

/// 把一组点聚合成 (avg, peak, `peak_time`)，空则全 None。
fn stat3(points: &[(DateTime<Utc>, f64)]) -> (Option<f64>, Option<f64>, Option<DateTime<Utc>>) {
    aggregate(points).map_or((None, None, None), |s| {
        (Some(s.avg), Some(s.peak), Some(s.peak_time))
    })
}

/// 序列分组 `key`：`host_ip` + `card_id`（C3 join 也复用此 key）。
pub(crate) fn series_key(s: &Series, spec: &DeviceSpec) -> String {
    let ip = extract_ip(&s.labels, &spec.labels.host_ip);
    let card = s
        .labels
        .get(&spec.card_id_label)
        .cloned()
        .unwrap_or_default();
    format!("{ip}|{card}")
}

/// 从标签取主机 IP：优先指定标签名，否则 instance 去端口。
pub(crate) fn extract_ip(labels: &HashMap<String, String>, prefer: &str) -> String {
    if let Some(v) = labels.get(prefer) {
        if !v.is_empty() {
            return v.clone();
        }
    }
    labels
        .get("instance")
        .map(|s| {
            // 剥端口：仅当末尾是 :<纯数字> 且 host 部分看起来像 IPv4 或
            // 方括号 IPv6 时才剥。裸 IPv6（如 "2001:db8::1"）的 rsplit_once(':')
            // 会误把末段当端口，需排除。
            if let Some((host, port)) = s.rsplit_once(':') {
                if !port.is_empty()
                    && port.chars().all(|c| c.is_ascii_digit())
                    && (host.starts_with('[') || host.contains('.'))
                {
                    // 剥 IPv6 方括号：[::1]:9090 → ::1
                    return host
                        .strip_prefix('[')
                        .and_then(|h| h.strip_suffix(']'))
                        .unwrap_or(host)
                        .to_string();
                }
            }
            s.clone()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppError;
    use crate::fetcher::MockFetcher;
    use chrono::TimeZone;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn cfg_with_mode(mode: &str) -> AppConfig {
        let mut cfg =
            serde_yaml::from_str::<AppConfig>(&crate::config::default_config_yaml()).unwrap();
        cfg.ownership.mode = mode.into();
        cfg
    }

    // ---- C2: fetch 失败转 Warning，不再静默 N/A ----

    #[tokio::test]
    async fn core_fetch_failure_produces_warning_not_silent() {
        // 核心指标查询返回错误；应记 Warning 且不 panic。
        let fetcher = MockFetcher::new().when(
            "DCGM_FI_DEV_GPU_UTIL",
            Err(AppError::Prometheus {
                source_name: "prod".into(),
                url: "http://x".into(),
                detail: "连接失败".into(),
            }),
        );
        let spec = DeviceSpec {
            display_name: "T".into(),
            core_util_metric: "DCGM_FI_DEV_GPU_UTIL".into(),
            memory: MemoryStrategy::composite_ratio("U", "F"),
            card_id_label: "gpu".into(),
            labels: crate::devices::LabelMapping {
                host_ip: "ip".into(),
                node_name: "node".into(),
                container: "c".into(),
                pod: "p".into(),
                namespace: "n".into(),
            },
        };
        let cfg = cfg_with_mode("instant");
        let out = collect_device(
            &fetcher,
            "prod",
            &spec,
            t(0),
            t(60),
            Duration::seconds(60),
            &cfg,
        )
        .await;
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("prod") && w.contains("连接失败")),
            "应有包含源名与失败原因的 Warning，实际：{:?}",
            out.warnings
        );
    }

    // ---- C3: HBM fallback 按 (ip,card_id) join，__name__ 不同也能对齐 ----

    #[tokio::test]
    async fn hbm_fallback_joins_by_card_id_despite_name_label() {
        // used/total 带不同 __name__（生产真实形态）；fallback 应仍能 join。
        let used = vec![Series {
            labels: labels(&[
                ("__name__", "npu_chip_info_hbm_used_memory"),
                ("id", "0"),
                ("ip", "1.1.1.1"),
            ]),
            points: vec![(t(0), 50.0), (t(60), 60.0)],
        }];
        let total = vec![Series {
            labels: labels(&[
                ("__name__", "npu_chip_info_hbm_total_memory"),
                ("id", "0"),
                ("ip", "1.1.1.1"),
            ]),
            points: vec![(t(0), 200.0), (t(60), 300.0)],
        }];

        let fetcher = MockFetcher::new()
            .when("npu_chip_info_utilization", Ok(vec![])) // 核心为空（聚焦 fallback）
            .when("npu_chip_info_hbm_utilization", Ok(vec![])) // direct 为空触发 fallback
            .when("hbm_used_memory", Ok(used))
            .when("hbm_total_memory", Ok(total));

        let spec = crate::devices::ascend_910b_spec();
        let cfg = cfg_with_mode("instant");
        let out = collect_device(
            &fetcher,
            "npu",
            &spec,
            t(0),
            t(60),
            Duration::seconds(60),
            &cfg,
        )
        .await;
        assert_eq!(out.records.len(), 1, "应产出 1 张卡");
        let r = &out.records[0];
        // 50/200=25, 60/300=20 → avg=22.5, peak=25
        assert!(r.mem_avg.unwrap() > 22.0 && r.mem_avg.unwrap() < 23.0);
        assert!((r.mem_peak.unwrap() - 25.0).abs() < 1e-9);
        assert!(r.core_avg.is_none(), "核心无数据应 N/A");
    }

    // ---- I7: 只有显存无核心，不产生幽灵空行；core=N/A 但身份来自 mem ----

    #[tokio::test]
    async fn card_with_only_memory_keeps_identity_from_mem() {
        // 核心: 一张卡 gpu=0；显存: 同一张卡。只测 mem 路径身份提取正确。
        let core = vec![Series {
            labels: labels(&[("gpu", "0"), ("ip", "1.1.1.1"), ("node", "n1")]),
            points: vec![(t(0), 10.0)],
        }];
        let mem = vec![Series {
            labels: labels(&[("gpu", "0"), ("ip", "1.1.1.1"), ("node", "n1")]),
            points: vec![(t(0), 20.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("DCGM_FI_DEV_GPU_UTIL", Ok(core))
            .when("ignoring(__name__)", Ok(mem)); // gpu_memory_promql 含 ignoring(__name__)
        let spec = crate::devices::nvidia_a10_spec();
        let cfg = cfg_with_mode("instant");
        let out = collect_device(
            &fetcher,
            "s",
            &spec,
            t(0),
            t(60),
            Duration::seconds(60),
            &cfg,
        )
        .await;
        assert_eq!(out.records.len(), 1);
        let r = &out.records[0];
        assert_eq!(r.card_id, "0");
        assert_eq!(r.host_ip, "1.1.1.1");
        assert_eq!(r.node_name, "n1");
    }

    // ---- C1: ownership 模式解析 ----

    #[test]
    fn ownership_mode_parses_known_strings() {
        assert_eq!(OwnershipMode::parse("instant"), OwnershipMode::Instant);
        assert_eq!(
            OwnershipMode::parse("last_in_range"),
            OwnershipMode::LastInRange
        );
        assert_eq!(OwnershipMode::parse("unknown"), OwnershipMode::LastInRange); // 未知回退
        assert_eq!(OwnershipMode::parse("  instant "), OwnershipMode::Instant); // 容错空白
    }

    // ---- C1: instant 模式归属来自 core 标签（向后兼容） ----

    #[tokio::test]
    async fn instant_mode_ownership_from_core_labels() {
        let core = vec![Series {
            labels: labels(&[
                ("gpu", "0"),
                ("ip", "1.1.1.1"),
                ("namespace", "ns1"),
                ("pod", "pod-a"),
                ("container", "c-a"),
            ]),
            points: vec![(t(0), 10.0)],
        }];
        let mem = vec![Series {
            labels: labels(&[("gpu", "0"), ("ip", "1.1.1.1")]),
            points: vec![(t(0), 20.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("DCGM_FI_DEV_GPU_UTIL", Ok(core))
            .when("ignoring(__name__)", Ok(mem));
        let spec = crate::devices::nvidia_a10_spec();
        let cfg = cfg_with_mode("instant");
        let out = collect_device(
            &fetcher,
            "s",
            &spec,
            t(0),
            t(60),
            Duration::seconds(60),
            &cfg,
        )
        .await;
        let r = &out.records[0];
        assert_eq!(r.namespace, "ns1");
        assert_eq!(r.pod, "pod-a");
        assert_eq!(r.container, "c-a");
    }

    // ---- C1: last_in_range 取末态非空归属 ----

    #[tokio::test]
    async fn last_in_range_picks_latest_nonempty_ownership() {
        // Pod 在窗口中途从 pod-a 漂移到 pod-b。实现：last_in_range 会用
        // `npu_chip_info_utilization{id="0"}` 重新查询，Pod 漂移产出两条 series
        // （pod-a 在 t0，pod-b 在 t60），取末态 pod-b。
        //
        // MockFetcher 按"首次命中子串"匹配：核心指标查询返回 pod-a（早期），
        // 但归属查询的 promql 同样含 "npu_chip_info_utilization"——为了让归属
        // 查询看到两条 series，我们用同一响应（含 pod-a + pod-b 两条），它同时
        // 满足核心聚合（pod-a 的点 10.0）与归属末态（pod-b 更晚）。
        let core_series = vec![
            Series {
                labels: labels(&[("id", "0"), ("ip", "1.1.1.1"), ("pod_name", "pod-a")]),
                points: vec![(t(0), 10.0)],
            },
            Series {
                labels: labels(&[("id", "0"), ("ip", "1.1.1.1"), ("pod_name", "pod-b")]),
                points: vec![(t(60), 30.0)],
            },
        ];
        let mem_direct: Vec<Series> = Vec::new(); // 触发 fallback 走 total
        let used = vec![Series {
            labels: labels(&[("__name__", "hbm_used"), ("id", "0"), ("ip", "1.1.1.1")]),
            points: vec![(t(0), 50.0)],
        }];
        let total = vec![Series {
            labels: labels(&[("__name__", "hbm_total"), ("id", "0"), ("ip", "1.1.1.1")]),
            points: vec![(t(0), 200.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("npu_chip_info_utilization", Ok(core_series))
            .when("npu_chip_info_hbm_utilization", Ok(mem_direct))
            .when("hbm_used_memory", Ok(used))
            .when("hbm_total_memory", Ok(total));
        let spec = crate::devices::ascend_910b_spec();
        let cfg = cfg_with_mode("last_in_range");
        let out = collect_device(
            &fetcher,
            "s",
            &spec,
            t(0),
            t(60),
            Duration::seconds(60),
            &cfg,
        )
        .await;
        assert_eq!(out.records.len(), 1);
        let r = &out.records[0];
        assert_eq!(r.pod, "pod-b", "last_in_range 应取末态 pod-b");
    }

    // ---- 稳定排序：多卡输出按 key 升序 ----

    #[tokio::test]
    async fn records_sorted_by_key() {
        // 故意以乱序提供三张卡，验证输出按 (ip,card_id) 升序。
        let make = |ip: &str, card: &str, val: f64| Series {
            labels: labels(&[("gpu", card), ("ip", ip)]),
            points: vec![(t(0), val)],
        };
        let core = vec![
            make("10.0.0.3", "0", 10.0),
            make("10.0.0.1", "2", 20.0),
            make("10.0.0.1", "1", 30.0),
        ];
        let fetcher = MockFetcher::new().when("DCGM_FI_DEV_GPU_UTIL", Ok(core));
        let spec = crate::devices::nvidia_a10_spec();
        let cfg = cfg_with_mode("instant");
        let out = collect_device(
            &fetcher,
            "s",
            &spec,
            t(0),
            t(60),
            Duration::seconds(60),
            &cfg,
        )
        .await;
        let keys: Vec<String> = out
            .records
            .iter()
            .map(|r| format!("{}|{}", r.host_ip, r.card_id))
            .collect();
        assert_eq!(
            keys,
            vec![
                "10.0.0.1|1".to_string(),
                "10.0.0.1|2".to_string(),
                "10.0.0.3|0".to_string(),
            ]
        );
    }

    // ---- merge_into 去重保留最后值 ----

    #[test]
    fn merge_into_dedup_keeps_last_value_at_same_timestamp() {
        use std::collections::HashMap;
        let mut slot = Some(Series {
            labels: HashMap::new(),
            points: vec![(t(0), 10.0), (t(60), 20.0)],
        });
        merge_into(
            &mut slot,
            Series {
                labels: HashMap::new(),
                points: vec![(t(60), 99.0), (t(120), 30.0)],
            },
        );
        let merged = slot.unwrap();
        assert_eq!(merged.points.len(), 3, "去重后应剩 3 个点");
        assert_eq!(merged.points[0], (t(0), 10.0));
        assert_eq!(
            merged.points[1], (t(60), 99.0),
            "同一时间戳应保留后者的值"
        );
        assert_eq!(merged.points[2], (t(120), 30.0));
    }

    // ---- instant 模式归属从 mem 标签回退 ----

    #[tokio::test]
    async fn instant_mode_ownership_falls_back_to_mem_labels() {
        // core 无归属标签，mem 带归属标签 → instant 模式应从 mem 取归属。
        let core = vec![Series {
            labels: labels(&[("gpu", "0"), ("ip", "1.1.1.1")]),
            points: vec![(t(0), 10.0)],
        }];
        let mem = vec![Series {
            labels: labels(&[
                ("gpu", "0"),
                ("ip", "1.1.1.1"),
                ("namespace", "ns-mem"),
                ("pod", "pod-mem"),
                ("container", "c-mem"),
            ]),
            points: vec![(t(0), 20.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("DCGM_FI_DEV_GPU_UTIL", Ok(core))
            .when("ignoring(__name__)", Ok(mem));
        let spec = crate::devices::nvidia_a10_spec();
        let cfg = cfg_with_mode("instant");
        let out = collect_device(
            &fetcher,
            "s",
            &spec,
            t(0),
            t(60),
            Duration::seconds(60),
            &cfg,
        )
        .await;
        let r = &out.records[0];
        assert_eq!(r.namespace, "ns-mem", "core 无归属时应从 mem 取 namespace");
        assert_eq!(r.pod, "pod-mem");
        assert_eq!(r.container, "c-mem");
    }

    #[tokio::test]
    async fn instant_mode_partial_fallback_per_field() {
        // core 有 namespace 但缺 pod/container，mem 有全部归属字段 →
        // namespace 从 core 取，pod/container 从 mem 取（逐字段回退）。
        let core = vec![Series {
            labels: labels(&[
                ("gpu", "0"),
                ("ip", "1.1.1.1"),
                ("namespace", "ns-core"),
            ]),
            points: vec![(t(0), 10.0)],
        }];
        let mem = vec![Series {
            labels: labels(&[
                ("gpu", "0"),
                ("ip", "1.1.1.1"),
                ("namespace", "ns-mem"),
                ("pod", "pod-mem"),
                ("container", "c-mem"),
            ]),
            points: vec![(t(0), 20.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("DCGM_FI_DEV_GPU_UTIL", Ok(core))
            .when("ignoring(__name__)", Ok(mem));
        let spec = crate::devices::nvidia_a10_spec();
        let cfg = cfg_with_mode("instant");
        let out = collect_device(
            &fetcher,
            "s",
            &spec,
            t(0),
            t(60),
            Duration::seconds(60),
            &cfg,
        )
        .await;
        let r = &out.records[0];
        assert_eq!(r.namespace, "ns-core", "core 有 namespace 应从 core 取");
        assert_eq!(r.pod, "pod-mem", "core 无 pod 应从 mem 回退");
        assert_eq!(r.container, "c-mem", "core 无 container 应从 mem 回退");
    }

    // ---- last_in_range 模式：核心无数据时回退到显存指标查归属 ----

    #[tokio::test]
    async fn last_in_range_falls_back_to_memory_metric_for_ownership() {
        // 核心指标查询返回空（模拟该卡只有显存数据），显存指标有归属标签。
        // last_in_range 应回退到显存指标查询归属。
        //
        // Mock 匹配规则：按子串首次命中。GPU 显存的 composite PromQL 含 "ignoring(__name__)"，
        // 所以下面 when("ignoring(__name__)", ...) 只匹配那个复合查询；而归属回退查询的是
        // 单独的 used 指标名 DCGM_FI_DEV_FB_USED{gpu="0"}，需要单独注册。
        let mem_series = vec![Series {
            labels: labels(&[
                ("gpu", "0"),
                ("ip", "1.1.1.1"),
                ("namespace", "ns-mem"),
                ("pod", "pod-mem"),
                ("container", "c-mem"),
            ]),
            points: vec![(t(0), 20.0), (t(60), 30.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("DCGM_FI_DEV_GPU_UTIL", Ok(vec![])) // 核心无数据
            .when("ignoring(__name__)", Ok(mem_series.clone()))      // 显存复合查询
            .when("DCGM_FI_DEV_FB_USED", Ok(mem_series)); // 归属回退查询 used 指标
        let spec = crate::devices::nvidia_a10_spec();
        let cfg = cfg_with_mode("last_in_range");
        let out = collect_device(
            &fetcher,
            "s",
            &spec,
            t(0),
            t(60),
            Duration::seconds(60),
            &cfg,
        )
        .await;
        // 只有显存的卡应产出记录，归属来自显存指标回退查询
        assert_eq!(out.records.len(), 1, "应有 1 张只有显存的卡");
        let r = &out.records[0];
        assert_eq!(r.namespace, "ns-mem", "last_in_range 应从显存指标取归属");
        assert_eq!(r.pod, "pod-mem");
        assert_eq!(r.container, "c-mem");
    }

    // ---- extract_ip IPv6 方括号 ----

    #[test]
    fn extract_ip_strips_ipv6_brackets() {
        let labels = HashMap::from([
            ("instance".into(), "[::1]:9090".into()),
        ]);
        assert_eq!(extract_ip(&labels, "ip"), "::1", "应剥去 IPv6 方括号");
    }

    #[test]
    fn extract_ip_strips_ipv6_full_brackets() {
        let labels = HashMap::from([
            ("instance".into(), "[2001:db8::1]:9090".into()),
        ]);
        assert_eq!(extract_ip(&labels, "ip"), "2001:db8::1");
    }

    #[test]
    fn extract_ip_bare_ipv6_no_port_unchanged() {
        // 裸 IPv6 无端口 → rsplit_once(':') 后 port 含非数字 → 不剥，原样返回
        let labels = HashMap::from([
            ("instance".into(), "2001:db8::1".into()),
        ]);
        assert_eq!(extract_ip(&labels, "ip"), "2001:db8::1");
    }

    // ---- node_name / host_ip 从 DeviceSpec.labels 取 ----

    #[tokio::test]
    async fn node_name_uses_label_from_spec() {
        // 验证 node_name 取自 spec.labels.node_name 指定的标签名，
        // 而非硬编码的 "node"。
        let core = vec![Series {
            labels: labels(&[
                ("gpu", "0"),
                ("ip", "1.1.1.1"),
                ("nodename", "my-node"), // 非标准 "node" 标签名
            ]),
            points: vec![(t(0), 10.0)],
        }];
        let mem = vec![Series {
            labels: labels(&[("gpu", "0"), ("ip", "1.1.1.1")]),
            points: vec![(t(0), 20.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("DCGM_FI_DEV_GPU_UTIL", Ok(core))
            .when("ignoring(__name__)", Ok(mem));
        let mut spec = crate::devices::nvidia_a10_spec();
        spec.labels.node_name = "nodename".into(); // 配置为 "nodename" 而非默认 "node"
        let cfg = cfg_with_mode("instant");
        let out = collect_device(
            &fetcher,
            "s",
            &spec,
            t(0),
            t(60),
            Duration::seconds(60),
            &cfg,
        )
        .await;
        let r = &out.records[0];
        assert_eq!(r.node_name, "my-node", "node_name 应取自 spec.labels.node_name 指定的标签");
    }

    #[tokio::test]
    async fn host_ip_uses_label_from_spec() {
        // 验证 host_ip 取自 spec.labels.host_ip 指定的标签名，
        // 而非顶层 cfg.host_ip.prefer_label。
        let core = vec![Series {
            labels: labels(&[
                ("gpu", "0"),
                ("host_address", "10.0.0.5"), // 非标准 "ip" 标签名
                ("node", "n1"),
            ]),
            points: vec![(t(0), 10.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("DCGM_FI_DEV_GPU_UTIL", Ok(core));
        let mut spec = crate::devices::nvidia_a10_spec();
        spec.labels.host_ip = "host_address".into(); // 配置为 "host_address"
        let cfg = cfg_with_mode("instant");
        let out = collect_device(
            &fetcher,
            "s",
            &spec,
            t(0),
            t(60),
            Duration::seconds(60),
            &cfg,
        )
        .await;
        let r = &out.records[0];
        assert_eq!(r.host_ip, "10.0.0.5", "host_ip 应取自 spec.labels.host_ip 指定的标签");
    }

    #[tokio::test]
    async fn node_name_empty_when_label_missing() {
        // 当标签中不存在 spec.labels.node_name 指定的键时，node_name 应为空串。
        let core = vec![Series {
            labels: labels(&[("gpu", "0"), ("ip", "1.1.1.1")]),
            points: vec![(t(0), 10.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("DCGM_FI_DEV_GPU_UTIL", Ok(core));
        let spec = crate::devices::nvidia_a10_spec(); // labels.node_name = "node"
        let cfg = cfg_with_mode("instant");
        let out = collect_device(
            &fetcher,
            "s",
            &spec,
            t(0),
            t(60),
            Duration::seconds(60),
            &cfg,
        )
        .await;
        let r = &out.records[0];
        assert_eq!(r.node_name, "", "标签中无 node 键时 node_name 应为空串");
    }

    // ---- last_in_range 多主机归属隔离：host_ip 过滤确保不跨主机取归属 ----

    #[tokio::test]
    #[allow(clippy::similar_names)]
    async fn last_in_range_isolates_ownership_by_host_ip() {
        // 两台主机各有 gpu=0 的卡，Pod 不同。last_in_range 归属查询应加
        // host_ip 过滤，确保每张卡只取本主机的归属，而非跨主机污染。
        //
        // 主机 A (1.1.1.1)：gpu=0, pod=pod-a（时间戳 t0）
        // 主机 B (2.2.2.2)：gpu=0, pod=pod-b（时间戳 t60，更晚）
        //
        // Mock 注册策略：用精确子串区分带不同 host_ip 的归属查询。
        // 核心指标查询匹配 "DCGM_FI_DEV_GPU_UTIL" 返回两条 series（用于聚合），
        // 但归属查询是带 ip 过滤的 PromQL，通过匹配更具体的子串返回单主机数据。
        let host_a_series = vec![Series {
            labels: labels(&[("gpu", "0"), ("ip", "1.1.1.1"), ("pod", "pod-a")]),
            points: vec![(t(0), 10.0)],
        }];
        let host_b_series = vec![Series {
            labels: labels(&[("gpu", "0"), ("ip", "2.2.2.2"), ("pod", "pod-b")]),
            points: vec![(t(60), 30.0)],
        }];
        // 核心查询（首次注册，按子串首次命中）：两条 series 都返回用于聚合
        let core_series = vec![
            host_a_series[0].clone(),
            host_b_series[0].clone(),
        ];
        let fetcher = MockFetcher::new()
            // 归属查询带 ip="1.1.1.1" → 匹配此注册（更具体的子串先注册）
            .when(r#"ip="1.1.1.1""#, Ok(host_a_series))
            // 归属查询带 ip="2.2.2.2" → 匹配此注册
            .when(r#"ip="2.2.2.2""#, Ok(host_b_series))
            // 核心指标查询（不含 ip= 过滤）→ 匹配此注册
            .when("DCGM_FI_DEV_GPU_UTIL", Ok(core_series));
        let spec = crate::devices::nvidia_a10_spec();
        let cfg = cfg_with_mode("last_in_range");
        let out = collect_device(
            &fetcher,
            "s",
            &spec,
            t(0),
            t(120),
            Duration::seconds(60),
            &cfg,
        )
        .await;
        assert_eq!(out.records.len(), 2, "应产出 2 张卡（2 台主机各 1 张）");
        let a = out.records.iter().find(|r| r.host_ip == "1.1.1.1").expect("应有主机 A");
        let b = out.records.iter().find(|r| r.host_ip == "2.2.2.2").expect("应有主机 B");
        assert_eq!(a.pod, "pod-a", "主机 A 的卡应归属 pod-a（不应被主机 B 的 pod-b 污染）");
        assert_eq!(b.pod, "pod-b", "主机 B 的卡应归属 pod-b");
    }
}
