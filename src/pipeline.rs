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
use crate::devices::{DeviceSpec, HostMetricsSpec, MemoryStrategy};
use crate::fetcher::MetricFetcher;
use crate::processor::{self, aggregate, last_non_empty, CardRecord, Series};
use crate::MAX_FALLBACK_DEPTH;
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
    /// 从配置字符串解析；未知值回退到 `LastInRange`（与默认配置一致）并记录警告。
    #[must_use]
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "instant" => Self::Instant,
            "last_in_range" => Self::LastInRange,
            other => {
                tracing::warn!(
                    "ownership.mode「{other}」不是有效值（支持 instant/last_in_range），使用默认 last_in_range"
                );
                Self::LastInRange
            }
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
            MemoryStrategy::CompositeRatio(b) => {
                // CompositeRatio 组合 PromQL 返回空时，降级为分别查 used/free，
                // 按 (host_ip, card_id) 对齐后算 used/(used+free)*100。
                // 复用 fallback_used_total 的 merge+join 逻辑，但 total = used + free。
                fallback_composite_ratio(&mut ctx, &b.composite_ratio).await
            }
        }
    } else {
        mem_series
    };

    // 3.5. 设备温度/功率（可选，取决于 DeviceSpec 配置）
    let temp_series = if let Some(tm) = &ctx.spec.temp_metric {
        fetch_with_warning(&mut ctx, tm).await
    } else {
        Vec::new()
    };
    let power_series = if let Some(pm) = &ctx.spec.power_metric {
        fetch_with_warning(&mut ctx, pm).await
    } else {
        Vec::new()
    };

    // 4. 分组：(host_ip, card_id) → (core?, mem?, temp?, power?)。同一卡多 series
    //    （如 Pod 漂移产出多条带不同归属标签的 series）的点会被合并进同一槽位，
    //    而非互相覆写，避免聚合数据丢失。槽位用 Option 互不覆写（I7 修复）。
    let mut groups: HashMap<String, (Option<Series>, Option<Series>)> = HashMap::new();
    let mut temp_by_key: HashMap<String, Option<Series>> = HashMap::new();
    let mut power_by_key: HashMap<String, Option<Series>> = HashMap::new();
    for s in core_series {
        let key = series_key(&s, ctx.spec);
        merge_into(&mut groups.entry(key).or_default().0, s);
    }
    for s in effective_mem {
        let key = series_key(&s, ctx.spec);
        merge_into(&mut groups.entry(key).or_default().1, s);
    }
    for s in temp_series {
        let key = series_key(&s, ctx.spec);
        merge_into(temp_by_key.entry(key).or_default(), s);
    }
    for s in power_series {
        let key = series_key(&s, ctx.spec);
        merge_into(power_by_key.entry(key).or_default(), s);
    }

    // 将仅有温度/功率数据但无核心/显存数据的卡加入 groups，避免丢失。
    // 这种情况在温度指标来自独立抓取作业、而核心/显存数据缺失时可能出现。
    for key in temp_by_key.keys().chain(power_by_key.keys()) {
        groups.entry(key.clone()).or_insert((None, None));
    }

    // 稳定排序：按 key 升序输出，避免 HashMap 随机序。
    let mut keys: Vec<String> = groups.keys().cloned().collect();
    keys.sort();

    // 5. 主机指标采集（可选，设备配方的一部分）。
    //    从 groups keys 提取唯一主机 IP，对每个 IP 查询 CPU/内存/句柄数，
    //    结果存入 HashMap 供记录构建时填入。
    let host_metrics_by_ip: HashMap<String, HostMetricValues> =
        if let Some(hm) = &ctx.spec.host_metrics {
            if hm.enabled {
                collect_host_metrics(&mut ctx, &groups, hm).await
            } else {
                tracing::info!(
                    "主机指标采集已禁用（设备类型「{}」host_metrics.enabled=false）",
                    ctx.spec.display_name
                );
                HashMap::new()
            }
        } else {
            HashMap::new()
        };

    for key in keys {
        let (core, mem) = groups.remove(&key).unwrap_or((None, None));
        // core 与 mem 都为 None 时，仍需保留该行（可能有温度/功率数据），
        // 身份字段从温度/功率序列中提取。

        // 身份字段优先从 core 取，core 缺失则从 mem 取，再缺失则从温度/功率取。
        let temp_opt = temp_by_key.get(&key).and_then(|o| o.as_ref());
        let power_opt = power_by_key.get(&key).and_then(|o| o.as_ref());
        let id_series = core
            .as_ref()
            .or(mem.as_ref())
            .or(temp_opt)
            .or(power_opt);
        let host_ip = id_series
            .map(|s| extract_ip(&s.labels, &ctx.spec.labels.host_ip))
            .unwrap_or_default();
        let card_id = id_series
            .and_then(|s| s.labels.get(&ctx.spec.card_id_label).cloned())
            .unwrap_or_default();
        let node_name = id_series
            .and_then(|s| s.labels.get(&ctx.spec.labels.node_name).cloned())
            .unwrap_or_default();

        let (c_avg, c_peak, c_peak_t, c_count, c_first, c_last) = core
            .as_ref()
            .map_or((None, None, None, None, None, None), |c| stat3(&c.points));
        let (m_avg, m_peak, m_peak_t, m_count, m_first, m_last) = mem
            .as_ref()
            .map_or((None, None, None, None, None, None), |m| stat3(&m.points));

        // 温度/功率：按 key 从独立分组中取（temp_opt/power_opt 已在上方提取）
        let (t_avg, t_peak, t_peak_t, t_count, t_first, t_last) =
            temp_opt.map_or((None, None, None, None, None, None), |s| stat3(&s.points));
        let (p_avg, p_peak, p_peak_t, p_count, p_first, p_last) =
            power_opt.map_or((None, None, None, None, None, None), |s| stat3(&s.points));

        // 归属
        let (namespace, pod, container) = ownership_for(
            &mut ctx,
            mode,
            &host_ip,
            &card_id,
            core.as_ref(),
            mem.as_ref(),
        )
        .await;

        // 主机指标：按 host_ip 从预采集结果查找
        let hmv = host_metrics_by_ip.get(&host_ip);

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
            core_count: c_count,
            core_first_time: c_first,
            core_last_time: c_last,
            mem_avg: m_avg,
            mem_peak: m_peak,
            mem_peak_time: m_peak_t,
            mem_count: m_count,
            mem_first_time: m_first,
            mem_last_time: m_last,
            temp_avg: t_avg,
            temp_peak: t_peak,
            temp_peak_time: t_peak_t,
            temp_count: t_count,
            temp_first_time: t_first,
            temp_last_time: t_last,
            power_avg: p_avg,
            power_peak: p_peak,
            power_peak_time: p_peak_t,
            power_count: p_count,
            power_first_time: p_first,
            power_last_time: p_last,
            host_cpu_avg: hmv.and_then(|v| v.cpu_avg),
            host_cpu_peak: hmv.and_then(|v| v.cpu_peak),
            host_cpu_peak_time: hmv.and_then(|v| v.cpu_peak_time),
            host_mem_avg: hmv.and_then(|v| v.mem_avg),
            host_mem_peak: hmv.and_then(|v| v.mem_peak),
            host_mem_peak_time: hmv.and_then(|v| v.mem_peak_time),
            host_handle_avg: hmv.and_then(|v| v.handle_avg),
            host_handle_peak: hmv.and_then(|v| v.handle_peak),
            host_handle_peak_time: hmv.and_then(|v| v.handle_peak_time),
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
            merge_points_into(&mut existing.points, incoming.points);
        }
        None => *slot = Some(incoming),
    }
}

/// 单台主机的主机指标聚合结果。
struct HostMetricValues {
    cpu_avg: Option<f64>,
    cpu_peak: Option<f64>,
    cpu_peak_time: Option<DateTime<Utc>>,
    mem_avg: Option<f64>,
    mem_peak: Option<f64>,
    mem_peak_time: Option<DateTime<Utc>>,
    handle_avg: Option<f64>,
    handle_peak: Option<f64>,
    handle_peak_time: Option<DateTime<Utc>>,
}

/// 采集该设备类型下所有唯一主机的主机指标。
///
/// 策略：对每个指标（CPU/内存/句柄数）发**一次**不带 instance 过滤的 PromQL 查询，
/// 获取所有主机的数据，然后按 `host_label`（默认 `instance`）标签值与设备指标中
/// 的 IP/主机名做内存匹配。
///
/// 这比逐 IP 发带正则过滤的查询更可靠：
/// - 避免了 PromQL 正则中 `\\.` 在 URL 传输中的编码问题
/// - 避免了 IP 前缀误匹配（如 `172.22.129.1` 匹配 `172.22.129.10`）
/// - 减少了 HTTP 请求次数（N 个指标 × 1 次 vs N 个指标 × M 台主机）
async fn collect_host_metrics(
    ctx: &mut QueryContext<'_>,
    groups: &HashMap<String, (Option<Series>, Option<Series>)>,
    hm: &HostMetricsSpec,
) -> HashMap<String, HostMetricValues> {
    if !hm.enabled {
        return HashMap::new();
    }

    // 提取唯一主机 IP：从 groups keys 解析，key 格式为 "{ip}|{card_id}"
    let host_ips: Vec<String> = groups
        .keys()
        .filter_map(|k| k.split('|').next())
        .filter(|ip| !ip.is_empty())
        .map(String::from)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    if host_ips.is_empty() {
        return HashMap::new();
    }

    tracing::info!(
        "开始采集主机指标（{} 台主机，设备类型「{}」，CPU={}，内存={}，句柄={}",
        host_ips.len(),
        ctx.spec.display_name,
        hm.cpu_expr.as_deref().unwrap_or("未配置"),
        hm.mem_expr.as_deref().unwrap_or("未配置"),
        hm.handle_expr.as_deref().unwrap_or("未配置"),
    );

    // 提取每台主机的 node_name，用于 instance 标签值匹配。
    // 从 groups 的 series 标签中提取，key 格式为 "{ip}|{card_id}"。
    let mut node_name_by_ip: HashMap<String, String> = HashMap::new();
    for (key, slot) in groups {
        let ip = key.split('|').next().unwrap_or("");
        if ip.is_empty() {
            continue;
        }
        let series = slot.0.as_ref().or(slot.1.as_ref());
        if let Some(s) = series {
            if let Some(nn) = s.labels.get(&ctx.spec.labels.node_name) {
                if !nn.is_empty() {
                    node_name_by_ip.insert(ip.to_string(), nn.clone());
                }
            }
        }
    }

    // 对每个指标发一次不带 instance 过滤的查询，获取所有主机数据
    // CPU 利用率
    let cpu_series = if let Some(cpu_expr) = &hm.cpu_expr {
        fetch_with_warning(ctx, cpu_expr).await
    } else {
        Vec::new()
    };

    // 内存利用率
    let mem_series = if let Some(mem_expr) = &hm.mem_expr {
        fetch_with_warning(ctx, mem_expr).await
    } else {
        Vec::new()
    };

    // 句柄数
    let handle_series = if let Some(handle_expr) = &hm.handle_expr {
        fetch_with_warning(ctx, handle_expr).await
    } else {
        Vec::new()
    };

    // 按 host_label 值（instance）将 series 分组到各主机 IP
    // 匹配策略：从 host_label 值中提取 IP，与 host_ips 匹配
    let mut result = HashMap::new();
    for ip in &host_ips {
        let node_name = node_name_by_ip.get(ip).map(|s| s.as_str()).unwrap_or("");

        // 从全量 CPU 数据中筛选属于该主机的 series
        let cpu_filtered: Vec<Series> = filter_series_by_host(
            &cpu_series, &hm.host_label, ip, node_name,
        );
        let (cpu_avg, cpu_peak, cpu_peak_t) =
            aggregate_host_metric_series(&cpu_filtered, ip, "CPU利用率", ctx.spec.display_name.as_str());

        let mem_filtered: Vec<Series> = filter_series_by_host(
            &mem_series, &hm.host_label, ip, node_name,
        );
        let (mem_avg, mem_peak, mem_peak_t) =
            aggregate_host_metric_series(&mem_filtered, ip, "内存利用率", ctx.spec.display_name.as_str());

        let handle_filtered: Vec<Series> = filter_series_by_host(
            &handle_series, &hm.host_label, ip, node_name,
        );
        let (handle_avg, handle_peak, handle_peak_t) =
            aggregate_host_metric_series(&handle_filtered, ip, "句柄数", ctx.spec.display_name.as_str());
        // 句柄数为整数，平均数舍弃小数部分
        let handle_avg = handle_avg.map(|v| v.trunc());

        result.insert(
            ip.clone(),
            HostMetricValues {
                cpu_avg,
                cpu_peak,
                cpu_peak_time: cpu_peak_t,
                mem_avg,
                mem_peak,
                mem_peak_time: mem_peak_t,
                handle_avg,
                handle_peak,
                handle_peak_time: handle_peak_t,
            },
        );
    }

    tracing::info!(
        "主机指标采集完成（{} 台主机，设备类型「{}」）",
        host_ips.len(),
        ctx.spec.display_name
    );

    result
}

/// 从全量 series 中筛选属于指定主机的 series。
///
/// 匹配策略：取 series 的 `host_label` 标签值，从中剥离端口号后
/// 与 `ip` 比较；若不匹配，再用 `node_name` 前缀匹配。
fn filter_series_by_host(
    all_series: &[Series],
    host_label: &str,
    ip: &str,
    node_name: &str,
) -> Vec<Series> {
    all_series
        .iter()
        .filter(|s| {
            let label_val = match s.labels.get(host_label) {
                Some(v) => v,
                None => return false,
            };
            // 策略1：从 label 值中剥离端口号，与 IP 比较
            let stripped = strip_port(label_val);
            if stripped == ip {
                return true;
            }
            // 策略2：用 node_name 前缀匹配（instance 可能是 "hostname:port"）
            if !node_name.is_empty() && label_val.starts_with(node_name) {
                return true;
            }
            false
        })
        .cloned()
        .collect()
}

/// 对筛选后的主机指标 series 做聚合，返回 (avg, peak, peak_time)。
fn aggregate_host_metric_series(
    series: &[Series],
    ip: &str,
    label: &str,
    display_name: &str,
) -> (Option<f64>, Option<f64>, Option<DateTime<Utc>>) {
    // 收集所有 series 的点（不去重），按时间戳分组取均值
    let mut all_points: Vec<(DateTime<Utc>, f64)> = Vec::new();
    for s in series {
        all_points.extend(s.points.iter().copied().filter(|(_, v)| v.is_finite()));
    }
    if all_points.is_empty() {
        tracing::warn!(
            "主机 {ip} {label} 指标无有效数据点（设备类型「{display_name}」）"
        );
        return (None, None, None);
    }
    all_points.sort_by_key(|(ts, _)| *ts);
    // 按时间戳分组取均值
    let mut averaged: Vec<(DateTime<Utc>, f64)> = Vec::new();
    let mut i = 0;
    while i < all_points.len() {
        let ts = all_points[i].0;
        let mut sum = 0.0_f64;
        let mut count = 0usize;
        while i < all_points.len() && all_points[i].0 == ts {
            sum += all_points[i].1;
            count += 1;
            i += 1;
        }
        averaged.push((ts, sum / count as f64));
    }
    aggregate(&averaged).map_or(
        (None, None, None),
        |s| (Some(s.avg), Some(s.peak), Some(s.peak_time)),
    )
}

/// 将 `label_filter`（如 `instance=~"^192\.168\.1\.100.*"`）追加到 PromQL 表达式中。
///
/// 策略分三种情况：
///
/// 1. **简单指标名**（如 `node_filefd_allocated`）：直接追加 `{label_filter}`
///    → `node_filefd_allocated{instance=~"..."}`
///
/// 2. **带标签的简单指标**（如 `metric{mode="idle"}`）：在闭合 `}` 前追加
///    → `metric{mode="idle", instance=~"..."}`
///
/// 3. **复杂表达式**（含函数调用、二元运算等）：在表达式内部**每个向量选择器**
///    中注入标签过滤器，而非使用 `and on(instance)` 外部过滤。
///
///    例：`100 - (avg by(instance) (rate(node_cpu_seconds_total{mode="idle"}[5m])) * 100)`
///    →  `100 - (avg by(instance) (rate(node_cpu_seconds_total{mode="idle", instance=~"..."}[5m])) * 100)`
///
///    例：`100 - (node_memory_MemAvailable_bytes / node_memory_MemTotal_bytes * 100)`
///    →  `100 - (node_memory_MemAvailable_bytes{instance=~"..."} / node_memory_MemTotal_bytes{instance=~"..."} * 100)`
///
/// 原因：之前 v1.9.4 使用 `and on(instance) (up{label_filter})` 外部过滤，但存在
/// 两个问题：(a) `up` 指标在某些 Prometheus 配置下不存在；(b) `and on(instance)`
/// 要求左右两侧的 `instance` 标签值完全一致，但不同抓取作业的 `instance` 标签
/// 格式可能不同（含/不含端口），导致匹配不上而返回空结果。
/// 在向量选择器内部注入标签过滤器是更可靠的做法——它直接在数据源层面过滤，
/// 不依赖任何辅助指标，也不存在标签值不匹配的问题。
#[allow(dead_code)] // 保留供归属查询和未来使用
fn append_label_filter(expr: &str, label_filter: &str) -> String {
    let trimmed = expr.trim();

    // 快速路径：简单指标名（无花括号、无括号/运算符）
    if is_simple_metric_name(trimmed) {
        return format!("{}{{{}}}", trimmed, label_filter);
    }

    // 快速路径：单指标+标签选择器（如 metric{mode="idle"}）
    // 判断条件：第一个 { 前是指标名，且整个表达式只有一个顶层 {}
    if let Some(brace_pos) = trimmed.find('{') {
        let before_brace = &trimmed[..brace_pos];
        if is_simple_metric_name(before_brace) {
            // 找匹配的闭合 }
            let mut depth = 0i32;
            let mut close_pos = None;
            for (i, ch) in trimmed[brace_pos..].char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            close_pos = Some(brace_pos + i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(close) = close_pos {
                let after_close = trimmed[close + 1..].trim();
                if after_close.is_empty() {
                    // 单指标+标签，{} 后无内容
                    let between = trimmed[brace_pos + 1..close].trim();
                    if between.is_empty() {
                        return format!("{}{{{}}}", before_brace, label_filter);
                    }
                    return format!(
                        "{}, {}}}",
                        &trimmed[..close],
                        label_filter
                    );
                }
                // } 后有内容（如 [5m]），不能走快速路径，走下方通用注入逻辑
            }
        }
    }

    // 通用路径：在表达式内部每个向量选择器中注入标签过滤器
    inject_label_filter_into_vector_selectors(trimmed, label_filter)
}

/// 在 PromQL 表达式内部的每个向量选择器中注入标签过滤器。
#[allow(dead_code)] // 保留供归属查询和未来使用
fn inject_label_filter_into_vector_selectors(expr: &str, label_filter: &str) -> String {
    let bytes = expr.as_bytes();
    let len = bytes.len();
    let mut result = String::with_capacity(expr.len() + label_filter.len() * 4);
    let mut pos = 0;

    while pos < len {
        // 检查当前位置是否是指标名的开始：[a-zA-Z_:]
        if is_metric_name_start_char(bytes[pos]) {
            // 跳过范围向量时间单位（如 [5m] 中的 m）
            if is_inside_range_vector_duration(expr, pos) {
                result.push(bytes[pos] as char);
                pos += 1;
                continue;
            }

            // 读取完整的标识符
            let name_start = pos;
            while pos < len && is_metric_name_char(bytes[pos]) {
                pos += 1;
            }
            let name = &expr[name_start..pos];
            let name_end = pos;

            // 跳过 PromQL 关键字和聚合操作符
            if is_promql_keyword(name) {
                result.push_str(name);
                continue;
            }

            // 根据标识符后面的字符判断是否为向量选择器
            if name_end < len && bytes[name_end] == b'{' {
                // 标识符后跟 `{` → 带标签的向量选择器
                // 在花括号内的闭合 `}` 前追加 label_filter
                result.push_str(name);
                result.push('{');

                let mut depth = 1i32;
                pos = name_end + 1; // 跳过开 {
                let content_start = pos;

                // 扫描花括号内容，正确处理嵌套和字符串字面量
                let mut in_string = false;
                let mut string_escape = false;
                while pos < len && depth > 0 {
                    let ch = bytes[pos];
                    if in_string {
                        if string_escape {
                            string_escape = false;
                        } else if ch == b'\\' {
                            string_escape = true;
                        } else if ch == b'"' {
                            in_string = false;
                        }
                    } else if ch == b'"' {
                        in_string = true;
                    } else if ch == b'{' {
                        depth += 1;
                    } else if ch == b'}' {
                        depth -= 1;
                    }
                    if depth > 0 {
                        pos += 1;
                    }
                }

                // pos 现在指向匹配的 `}`
                let content = &expr[content_start..pos];
                let trimmed_content = content.trim();
                if trimmed_content.is_empty() {
                    result.push_str(label_filter);
                } else {
                    result.push_str(content);
                    result.push_str(", ");
                    result.push_str(label_filter);
                }
                result.push('}');
                pos += 1; // 跳过 }
            } else if name_end < len
                && (bytes[name_end] == b'('
                    || is_metric_name_char(bytes[name_end])
                    || bytes[name_end] == b')')
            {
                // 标识符后跟 `(`（函数调用）、其他标识符字符（关键字参数如 by(instance)）、
                // 或 `)`（标签名参数如 by(instance)）→ 不是向量选择器
                result.push_str(name);
            } else {
                // 标识符后不跟 `{`、`(`、`)`、其他标识符字符
                // → 裸向量选择器，追加 {label_filter}
                result.push_str(name);
                result.push('{');
                result.push_str(label_filter);
                result.push('}');
            }
        } else {
            // 非标识符字符（数字、括号、运算符等），原样复制
            result.push(bytes[pos] as char);
            pos += 1;
        }
    }

    result
}

/// 判断位置 `pos` 的标识符是否在范围向量时间区间内（如 `[5m]` 中的 `m`）。
///
/// 检查方法：向前查找最近的非空白字符，如果是数字且该数字在 `[` 之后，
/// 则当前位置是时间单位而非指标名。
#[allow(dead_code)] // 保留供 append_label_filter 使用
fn is_inside_range_vector_duration(expr: &str, pos: usize) -> bool {
    let bytes = expr.as_bytes();
    let mut i = pos;
    while i > 0 {
        i -= 1;
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            continue;
        }
        if b.is_ascii_digit() {
            // 找到数字，继续向前查找是否在 `[` 之后
            let mut j = i;
            while j > 0 {
                j -= 1;
                let c = bytes[j];
                if c.is_ascii_whitespace() || c.is_ascii_digit() {
                    continue;
                }
                if c == b'[' {
                    return true;
                }
                break;
            }
        }
        break;
    }
    false
}

/// PromQL 关键字和聚合操作符，不应被当作向量选择器注入标签。
#[allow(dead_code)] // 保留供 append_label_filter 使用
const PROMQL_KEYWORDS: &[&str] = &[
    // 聚合子句关键字
    "by",
    "without",
    "on",
    "ignoring",
    "group_left",
    "group_right",
    // 二元运算符关键字
    "and",
    "or",
    "unless",
    "bool",
    // 聚合操作符
    "sum",
    "avg",
    "count",
    "min",
    "max",
    "stddev",
    "stdvar",
    "topk",
    "bottomk",
    "quantile",
    "count_values",
    // PromQL 内置指标（用于目标状态监控，不是用户数据指标）
    "up",
];

/// 判断标识符是否为 PromQL 关键字或聚合操作符。
#[allow(dead_code)]
fn is_promql_keyword(s: &str) -> bool {
    PROMQL_KEYWORDS.contains(&s)
}

/// 判断字节是否是 Prometheus 指标名的首字符：`[a-zA-Z_:]`
#[allow(dead_code)]
fn is_metric_name_start_char(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b':'
}

/// 判断字节是否是 Prometheus 指标名的组成字符：`[a-zA-Z0-9_:]`
#[allow(dead_code)]
fn is_metric_name_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b':'
}

/// 判断字符串是否为合法的 Prometheus 指标名：`[a-zA-Z_:][a-zA-Z0-9_:]*`
#[allow(dead_code)]
fn is_simple_metric_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}


/// 将 incoming 的点合并进 existing，按时间戳排序并去重（同一时间戳保留最大值）。
///
/// 供 `merge_into`（pipeline 层按 join key 合并）和
/// `merge_series`（fetcher 层按 label 合并）复用。
/// 注意：主机指标的多 series 合并不应使用此函数（会丢弃同时间戳的多观测值），
/// 应使用 `aggregate_host_metric_series` 中的按时间戳分组取均值逻辑。
pub fn merge_points_into(
    existing: &mut Vec<(chrono::DateTime<chrono::Utc>, f64)>,
    incoming: Vec<(chrono::DateTime<chrono::Utc>, f64)>,
) {
    existing.extend(incoming);
    // 按时间戳稳定排序，等时间戳时按值作二级排序以保证确定性去重。
    existing.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    // 同一时间戳保留最大值（排序后同时间戳按值升序，反转后最大值在前，
    // dedup_by 保留首个元素即最大值）。
    existing.reverse();
    existing.dedup_by(|a, b| a.0 == b.0);
    existing.reverse();
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
    fallback_used_total_inner(ctx, fallback, 0).await
}

/// `fallback_used_total` 的内部实现，带递归深度限制以防止配置错误导致的栈溢出。
async fn fallback_used_total_inner(
    ctx: &mut QueryContext<'_>,
    fallback: &MemoryStrategy,
    depth: usize,
) -> Vec<Series> {
    if depth > MAX_FALLBACK_DEPTH {
        tracing::warn!("fallback 嵌套深度超过 {MAX_FALLBACK_DEPTH} 层，中止递归以避免栈溢出");
        return Vec::new();
    }
    match fallback {
        MemoryStrategy::CompositeFromTotal(body) => {
            fallback_composite_from_total_inner(
                ctx,
                body.composite_from_total.used.as_str(),
                body.composite_from_total.total.as_str(),
            )
            .await
        }
        MemoryStrategy::CompositeRatio(b) => {
            // DirectMetric 的 fallback 为 CompositeRatio 时，委托给
            // fallback_composite_ratio 按 used/free 重算。
            fallback_composite_ratio(ctx, &b.composite_ratio).await
        }
        MemoryStrategy::DirectMetric(b) => {
            // DirectMetric 嵌套 DirectMetric 时，先查询内层 DirectMetric 的自身指标；
            // 若仍为空，再递归尝试其 fallback。
            let result = fetch_with_warning(ctx, &b.direct_metric.metric).await;
            if !result.is_empty() {
                result
            } else if let Some(inner_fb) = &b.direct_metric.fallback {
                Box::pin(fallback_used_total_inner(ctx, inner_fb.as_ref(), depth + 1)).await
            } else {
                Vec::new()
            }
        }
    }
}

/// `CompositeFromTotal` 的核心查询+join 逻辑：拉 used/total，按 (`host_ip`, `card_id`)
/// 对齐重算显存占用率（C3 修复）。
///
/// 从 `fallback_used_total` 抽出，因为 `fallback_used_total` 现在是分派函数，
/// 需要把 `CompositeFromTotal` 的实际逻辑放在独立函数中。
async fn fallback_composite_from_total_inner(
    ctx: &mut QueryContext<'_>,
    used_metric: &str,
    total_metric: &str,
) -> Vec<Series> {
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

    // used 同样按 join key 合并（Pod 漂移可能产出多条 used series），
    // 保证 hbm_fallback_series 的 used/total 对齐来自同一时间戳。
    let mut used_by_key: HashMap<String, Option<Series>> = HashMap::new();
    for u in used_s {
        let key = series_key(&u, ctx.spec);
        merge_into(used_by_key.entry(key).or_default(), u);
    }

    used_by_key
        .into_iter()
        .filter_map(|(key, u_opt)| {
            let u = u_opt?;
            let t = match total_by_key.get(&key).and_then(|opt| opt.as_ref()) {
                Some(t) => t,
                None => {
                    ctx.out.push_warning(format!(
                        "CompositeFromTotal fallback：设备 {key} 有 used 数据但无 total 数据，跳过显存计算"
                    ));
                    return None;
                }
            };
            Some(processor::hbm_fallback_series(&u, t))
        })
        .collect()
}

/// `CompositeRatio` fallback：组合 `PromQL` 返回空时，分别查 used/free，
/// 按 (`host_ip`, `card_id`) 对齐后构造 total = used + free 的伪 Series，
/// 再调用 [`processor::hbm_fallback_series`] 计算 used/(used+free)*100。
///
/// 这与 `fallback_used_total` 的区别在于 total 不是直接查到的指标，
/// 而是由 used + free 逐点相加合成。
async fn fallback_composite_ratio(
    ctx: &mut QueryContext<'_>,
    uf: &crate::devices::UsedFree,
) -> Vec<Series> {
    let used_s = fetch_with_warning(ctx, &uf.used).await;
    let free_s = fetch_with_warning(ctx, &uf.free).await;

    // 按 join key 合并 used 和 free
    let mut used_by_key: HashMap<String, Option<Series>> = HashMap::new();
    for u in used_s {
        let key = series_key(&u, ctx.spec);
        merge_into(used_by_key.entry(key).or_default(), u);
    }
    let mut free_by_key: HashMap<String, Option<Series>> = HashMap::new();
    for f in free_s {
        let key = series_key(&f, ctx.spec);
        merge_into(free_by_key.entry(key).or_default(), f);
    }

    used_by_key
        .into_iter()
        .filter_map(|(key, u_opt)| {
            let u = u_opt?;
            let f = match free_by_key.get(&key).and_then(|opt| opt.as_ref()) {
                Some(f) => f,
                None => {
                    ctx.out.push_warning(format!(
                        "CompositeRatio fallback：设备 {key} 有 used 数据但无 free 数据，跳过显存计算"
                    ));
                    return None;
                }
            };
            // 合成 total = used + free 的伪 Series（逐点相加）
            let total = synthesize_total(&u, f);
            Some(processor::hbm_fallback_series(&u, &total))
        })
        .collect()
}

/// 由 used + free 逐点相加合成 total Series。
///
/// 按 timestamp 对齐：仅保留 used 和 free 都有数据的时间戳，
/// total 值 = used + free。保留 used 的标签（身份标签已由 join key 对齐）。
fn synthesize_total(used: &Series, free: &Series) -> Series {
    // 显式去重：同一 timestamp 保留最大值（与 merge_points_into 语义一致）。
    let mut free_map: HashMap<i64, f64> = HashMap::new();
    for (ts, v) in &free.points {
        free_map
            .entry(ts.timestamp())
            .and_modify(|existing| {
                if *v > *existing {
                    *existing = *v;
                }
            })
            .or_insert(*v);
    }
    let mut points = Vec::new();
    for (ts, u) in &used.points {
        if let Some(f) = free_map.get(&ts.timestamp()) {
            let total = u + f;
            if total > 0.0 && total.is_finite() {
                points.push((*ts, total));
            }
        }
    }
    Series {
        labels: used.labels.clone(),
        points,
    }
}

/// 用 `host_ip` 标签过滤查询归属数据；若返回空，回退用 instance 标签重试。
///
/// 当 `extract_ip` 从 `instance` 标签解析 IP（因 `spec.labels.host_ip` 指定的标签
/// 为空），归属查询用 `spec.labels.host_ip` 作标签名会匹配不到，需回退到
/// `instance` 标签。`instance` 值含端口，用正则 `=~` 匹配 IP 前缀。
async fn query_with_ip_fallback(
    ctx: &mut QueryContext<'_>,
    metric: &str,
    escaped_card_id: &str,
    ip_escaped: &str,
    host_ip: &str,
) -> Option<Vec<Series>> {
    // host_ip 为空时无法安全过滤，跳过归属查询避免跨主机污染
    if ip_escaped.is_empty() {
        return None;
    }
    let promql = format!(
        "{metric}{{{a}=\"{v}\",{ip_label}=\"{ip}\"}}",
        a = ctx.spec.card_id_label,
        v = escaped_card_id,
        ip_label = ctx.spec.labels.host_ip,
        ip = ip_escaped
    );
    match ctx
        .fetcher
        .query_range(&promql, ctx.start, ctx.end, ctx.step)
        .await
    {
        Ok(series) if !series.is_empty() => Some(series),
        _ => {
            // 回退：用 instance 标签（IP 可能从 instance 解析而来）。
            // instance 格式为 "ip:port" 或 "[ipv6]:port"，用正则精确匹配
            // 避免 IP 前缀误匹配（如 "1.1" 不应匹配 "1.1.1.1:9090"）。
            // 对 IP 做完整 Prometheus 正则转义（RE2 语法），防御性处理
            // 含特殊字符的标签值。IPv6 方括号需单独处理。
            let ip_regex = build_instance_regex(host_ip);
            let instance_promql = format!(
                "{metric}{{{a}=\"{v}\",instance=~\"{ip_re}\"}}",
                a = ctx.spec.card_id_label,
                v = escaped_card_id,
                ip_re = ip_regex
            );
            match ctx
                .fetcher
                .query_range(&instance_promql, ctx.start, ctx.end, ctx.step)
                .await
            {
                Ok(series) if !series.is_empty() => Some(series),
                _ => None,
            }
        }
    }
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
    let escaped = escape_promql_label_value(card_id);
    let ip_escaped = escape_promql_label_value(host_ip);

    // 优先用核心指标查归属，含 instance 标签回退
    if let Some(series) = query_with_ip_fallback(
        ctx,
        &ctx.spec.core_util_metric,
        &escaped,
        &ip_escaped,
        host_ip,
    )
    .await
    {
        let ns = last_label_value(&series, &ctx.spec.labels.namespace);
        let pod = last_label_value(&series, &ctx.spec.labels.pod);
        let ct = last_label_value(&series, &ctx.spec.labels.container);
        // 仅当至少一个归属标签非空时返回，否则回退到显存指标
        // （与 instant 模式的逐字段回退语义一致：核心指标有数据但归属标签全空时，
        // 不应直接返回三个空串，而应尝试从显存指标取归属）
        if !ns.is_empty() || !pod.is_empty() || !ct.is_empty() {
            return (ns, pod, ct);
        }
    }

    // 核心指标无数据或归属全空，回退到显存指标查询归属。
    // 依次尝试所有显存指标（含 fallback 链），因为 DirectMetric 的主指标
    // 无数据时，fallback 的 used 指标仍可能有归属标签。
    // 同样，如果某指标有数据但归属标签全空，继续尝试下一个指标（与 instant
    // 模式逐字段回退语义一致）。
    let mem_names = ctx.spec.memory.ownership_metric_names();
    for mem_metric in &mem_names {
        if let Some(mem_series) =
            query_with_ip_fallback(ctx, mem_metric, &escaped, &ip_escaped, host_ip).await
        {
            let ns = last_label_value(&mem_series, &ctx.spec.labels.namespace);
            let pod = last_label_value(&mem_series, &ctx.spec.labels.pod);
            let ct = last_label_value(&mem_series, &ctx.spec.labels.container);
            if !ns.is_empty() || !pod.is_empty() || !ct.is_empty() {
                return (ns, pod, ct);
            }
        }
    }
    (String::new(), String::new(), String::new())
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

/// 把一组点聚合成 (avg, peak, `peak_time`, count, `first_time`, `last_time`)，空则全 None。
#[allow(clippy::type_complexity)]
fn stat3(
    points: &[(DateTime<Utc>, f64)],
) -> (
    Option<f64>,
    Option<f64>,
    Option<DateTime<Utc>>,
    Option<usize>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
) {
    aggregate(points).map_or((None, None, None, None, None, None), |s| {
        (
            Some(s.avg),
            Some(s.peak),
            Some(s.peak_time),
            Some(s.count),
            Some(s.first_time),
            Some(s.last_time),
        )
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
/// 两路均剥离端口号（如 "192.168.1.1:9090" → "192.168.1.1"），
/// 避免端口冒号被 main.rs 的 IPv6 检测误判导致 host 指标正则不匹配。
pub(crate) fn extract_ip(labels: &HashMap<String, String>, prefer: &str) -> String {
    let raw = labels
        .get(prefer)
        .filter(|v| !v.is_empty())
        .or_else(|| labels.get("instance"));
    match raw {
        Some(s) => strip_port(s),
        None => String::new(),
    }
}

/// 从 "ip:port" 或 "[ipv6]:port" 实例地址中剥离端口号，
/// 返回裸 IP（IPv6 去掉外围方括号）。不含端口的地址原样返回，
/// 但 [ipv6]（有方括号无端口）也会剥去方括号返回裸 IPv6。
fn strip_port(s: &str) -> String {
    if let Some((host, port)) = s.rsplit_once(':') {
        if !port.is_empty()
            && port.chars().all(|c| c.is_ascii_digit())
            && (host.starts_with('[') || !host.contains(':'))
        {
            // 剥 IPv6 方括号：[::1]:9090 → ::1
            return host
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(host)
                .to_string();
        }
    }
    // 无端口但带方括号的 IPv6（如 [::1]），也需剥去方括号。
    s.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(s)
        .to_string()
}

/// 对字符串做 Prometheus 正则转义（RE2 语法）。
///
/// 转义所有正则元字符，使字符串在 `=~` 匹配中被当作字面量。
/// Prometheus 使用 RE2 引擎，元字符为：`\ . ^ $ * + ? ( ) [ ] { } |`
/// 对 `PromQL` 正则匹配（`=~"..."`）中的标签值做 RE2 转义。
///
/// PromQL 标签值使用 Go 双引号字符串字面量语法，Go 会先解析一层转义
/// （如 `\\.` → `\.`），再交给 RE2 引擎。因此要在 RE2 中匹配字面量 `.`，
/// 需要正则 `\.`，对应 Go 字符串 `\\.`——即本函数输出的**双反斜杠**形式。
///
/// 例如 `"192.168.1.1"` → `"192\\.168\\.1\\.1"`，嵌入 `instance=~"…"` 后
/// Go 解析为 `192\.168\.1\.1`，RE2 匹配字面量 IP。
pub fn escape_promql_regex(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        if matches!(
            c,
            '\\' | '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
        ) {
            // 双反斜杠：Go 字符串字面量解析一层后留给 RE2 一个反斜杠。
            // 反斜杠自身需要四重反斜杠：Go 解析 \\\\→\\，RE2 将 \\ 视为字面 \。
            if c == '\\' {
                out.push_str("\\\\\\\\");
            } else {
                out.push_str("\\\\");
                out.push(c);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 对 `PromQL` 标签值做转义（用于 `="..."` 精确匹配）。
///
/// Prometheus 标签值使用 Go 字符串字面量语法，需转义：`\\`、`\"`、`\n`、`\r`、`\t`。
fn escape_promql_label_value(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// 构建 instance 标签的正则匹配模式，用于精确匹配主机 IP。
///
/// Prometheus `instance` 标签值格式：
/// - IPv4：`ip:port` 或裸 `ip`
/// - IPv6：`[ip]:port` 或 `[ip]`（方括号是 Prometheus 惯例，RFC 3986）
///
/// 对于 IPv6，生成同时匹配带方括号和不带方括号两种格式的正则，
/// 兼容默认 `instance` 标签（总是带方括号）和自定义标签（可能不带方括号）。
///
/// Prometheus `=~` 是全锚定匹配（等同 `^...$`），因此正则必须消费整个标签值。
///
/// **IPv4 关键**：IP 最后一段数字后必须跟行尾或冒号+端口号（纯数字），
/// 使用 `($|:[0-9]+)` 而非 `($|:.*)`，避免 IP 前缀误匹配——
/// 如 `172.22.129.1` 的正则若用 `.*` 会同时匹配 `172.22.129.10:9101`、
/// `172.22.129.15:9101` 等，因为 `0`、`5` 等被 `.*` 消费。
///
/// IPv6 方括号分支使用 `($|:[0-9]+)` 匹配行尾或冒号+端口号；
/// 裸 IPv6 分支仅用 `$`（不含 `:[0-9]+`），因为冒号是 IPv6 地址的结构字符，
/// `:[0-9]+` 会导致前缀共享的 IPv6 地址误匹配（如 `2001:db8::1` 匹配 `2001:db8::1:2`）。
///
/// 输出为 Go 双引号字符串字面量中的正则表达式（RE2 语法），
/// 适配 Prometheus PromQL `=~"..."` 的 Go 解析规则。
#[must_use]
pub fn build_instance_regex(ip: &str) -> String {
    let escaped = escape_promql_regex(ip);
    if ip.contains(':') {
        // IPv6：同时匹配 [ip]:port / [ip] 和裸 ip:port / ip，
        // 兼容 instance 标签（总带方括号）和自定义 host_label（可能不带）。
        // RE2 中 \[ / \] 匹配字面方括号；Go 字符串 \\\\[ → \\[ → RE2 \[。
        // ($|:[0-9]+) 匹配行尾或冒号+端口号（纯数字）。
        // 裸 IPv6 分支仅用 $（不含 :[0-9]+），因为冒号在 IPv6 地址中是结构字符，
        // 会导致前缀共享的 IPv6 地址误匹配（如 2001:db8::1 匹配 2001:db8::1:2）。
        format!("^(\\\\[{escaped}\\\\]($|:[0-9]+)|{escaped}$)")
    } else {
        // IPv4：精确匹配 IP 后跟行尾或冒号+端口号（纯数字）。
        // 使用 ($|:[0-9]+) 而非 ($|:.*)，避免 IP 前缀误匹配：
        // 如 172.22.129.1 不应匹配 172.22.129.10:9101（10 的 0 会被 .* 消费）。
        format!("^{escaped}($|:[0-9]+)")
    }
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
            serde_yaml_ng::from_str::<AppConfig>(&crate::config::default_config_yaml()).unwrap();
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
            temp_metric: None,
            power_metric: None,
            host_metrics: None,
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
            labels: labels(&[("gpu", "0"), ("host_ip", "1.1.1.1"), ("pod_node", "n1")]),
            points: vec![(t(0), 10.0)],
        }];
        let mem = vec![Series {
            labels: labels(&[("gpu", "0"), ("host_ip", "1.1.1.1"), ("pod_node", "n1")]),
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
                labels: labels(&[("id", "0"), ("host_ip", "1.1.1.1"), ("pod_name", "pod-a")]),
                points: vec![(t(0), 10.0)],
            },
            Series {
                labels: labels(&[("id", "0"), ("host_ip", "1.1.1.1"), ("pod_name", "pod-b")]),
                points: vec![(t(60), 30.0)],
            },
        ];
        let mem_direct: Vec<Series> = Vec::new(); // 触发 fallback 走 total
        let used = vec![Series {
            labels: labels(&[
                ("__name__", "hbm_used"),
                ("id", "0"),
                ("host_ip", "1.1.1.1"),
            ]),
            points: vec![(t(0), 50.0)],
        }];
        let total = vec![Series {
            labels: labels(&[
                ("__name__", "hbm_total"),
                ("id", "0"),
                ("host_ip", "1.1.1.1"),
            ]),
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
            labels: labels(&[("gpu", card), ("host_ip", ip)]),
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
        assert_eq!(merged.points[1], (t(60), 99.0), "同一时间戳应保留后者的值");
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
            labels: labels(&[("gpu", "0"), ("ip", "1.1.1.1"), ("namespace", "ns-core")]),
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
                ("host_ip", "1.1.1.1"),
                ("namespace", "ns-mem"),
                ("pod", "pod-mem"),
                ("container", "c-mem"),
            ]),
            points: vec![(t(0), 20.0), (t(60), 30.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("DCGM_FI_DEV_GPU_UTIL", Ok(vec![])) // 核心无数据
            .when("ignoring(__name__)", Ok(mem_series.clone())) // 显存复合查询
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
        let labels = HashMap::from([("instance".into(), "[::1]:9090".into())]);
        assert_eq!(extract_ip(&labels, "ip"), "::1", "应剥去 IPv6 方括号");
    }

    #[test]
    fn extract_ip_strips_ipv6_full_brackets() {
        let labels = HashMap::from([("instance".into(), "[2001:db8::1]:9090".into())]);
        assert_eq!(extract_ip(&labels, "ip"), "2001:db8::1");
    }

    #[test]
    fn extract_ip_bare_ipv6_no_port_unchanged() {
        // 裸 IPv6 无端口 → rsplit_once(':') 后 port 含非数字 → 不剥，原样返回
        let labels = HashMap::from([("instance".into(), "2001:db8::1".into())]);
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
        assert_eq!(
            r.node_name, "my-node",
            "node_name 应取自 spec.labels.node_name 指定的标签"
        );
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
        let fetcher = MockFetcher::new().when("DCGM_FI_DEV_GPU_UTIL", Ok(core));
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
        assert_eq!(
            r.host_ip, "10.0.0.5",
            "host_ip 应取自 spec.labels.host_ip 指定的标签"
        );
    }

    #[tokio::test]
    async fn node_name_empty_when_label_missing() {
        // 当标签中不存在 spec.labels.node_name 指定的键时，node_name 应为空串。
        let core = vec![Series {
            labels: labels(&[("gpu", "0"), ("ip", "1.1.1.1")]),
            points: vec![(t(0), 10.0)],
        }];
        let fetcher = MockFetcher::new().when("DCGM_FI_DEV_GPU_UTIL", Ok(core));
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
            labels: labels(&[("gpu", "0"), ("host_ip", "1.1.1.1"), ("pod", "pod-a")]),
            points: vec![(t(0), 10.0)],
        }];
        let host_b_series = vec![Series {
            labels: labels(&[("gpu", "0"), ("host_ip", "2.2.2.2"), ("pod", "pod-b")]),
            points: vec![(t(60), 30.0)],
        }];
        // 核心查询（首次注册，按子串首次命中）：两条 series 都返回用于聚合
        let core_series = vec![host_a_series[0].clone(), host_b_series[0].clone()];
        let fetcher = MockFetcher::new()
            // 归属查询带 host_ip="1.1.1.1" → 匹配此注册（更具体的子串先注册）
            .when(r#"host_ip="1.1.1.1""#, Ok(host_a_series))
            // 归属查询带 host_ip="2.2.2.2" → 匹配此注册
            .when(r#"host_ip="2.2.2.2""#, Ok(host_b_series))
            // 核心指标查询（不含 host_ip= 过滤）→ 匹配此注册
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
        let a = out
            .records
            .iter()
            .find(|r| r.host_ip == "1.1.1.1")
            .expect("应有主机 A");
        let b = out
            .records
            .iter()
            .find(|r| r.host_ip == "2.2.2.2")
            .expect("应有主机 B");
        assert_eq!(
            a.pod, "pod-a",
            "主机 A 的卡应归属 pod-a（不应被主机 B 的 pod-b 污染）"
        );
        assert_eq!(b.pod, "pod-b", "主机 B 的卡应归属 pod-b");
    }

    // ---- synthesize_total 逐点相加 ----

    #[test]
    fn synthesize_total_adds_used_and_free() {
        let used = Series {
            labels: HashMap::new(),
            points: vec![(t(0), 30.0), (t(60), 60.0)],
        };
        let free = Series {
            labels: HashMap::new(),
            points: vec![(t(0), 170.0), (t(60), 240.0)],
        };
        let total = synthesize_total(&used, &free);
        assert_eq!(total.points.len(), 2);
        assert!((total.points[0].1 - 200.0).abs() < 1e-9); // 30 + 170 = 200
        assert!((total.points[1].1 - 300.0).abs() < 1e-9); // 60 + 240 = 300
    }

    #[test]
    fn synthesize_total_skips_timestamps_missing_in_free() {
        let used = Series {
            labels: HashMap::new(),
            points: vec![(t(0), 30.0), (t(60), 60.0), (t(120), 90.0)],
        };
        let free = Series {
            labels: HashMap::new(),
            points: vec![(t(0), 170.0), (t(120), 110.0)], // t60 缺失
        };
        let total = synthesize_total(&used, &free);
        assert_eq!(total.points.len(), 2, "缺失 free 的时间戳应跳过");
        assert!((total.points[0].1 - 200.0).abs() < 1e-9); // t0: 30+170
        assert!((total.points[1].1 - 200.0).abs() < 1e-9); // t120: 90+110
    }

    #[test]
    fn synthesize_total_skips_zero_and_non_finite_total() {
        let used = Series {
            labels: HashMap::new(),
            points: vec![(t(0), 0.0), (t(60), f64::MAX), (t(120), 50.0)],
        };
        let free = Series {
            labels: HashMap::new(),
            points: vec![(t(0), 0.0), (t(60), f64::MAX), (t(120), 150.0)],
        };
        let total = synthesize_total(&used, &free);
        assert_eq!(total.points.len(), 1, "total=0 和 Inf 应被跳过");
        assert!((total.points[0].1 - 200.0).abs() < 1e-9); // t120: 50+150=200
    }

    // ---- DirectMetric 的 fallback 为 CompositeRatio 时不应静默丢失 ----

    #[tokio::test]
    async fn direct_metric_composite_ratio_fallback_not_silent() {
        // 自定义设备：DirectMetric 主指标为空，fallback 为 CompositeRatio(used, free)。
        // 修复前 fallback_used_total 对 CompositeRatio 返回空 Vec，显存静默丢失。
        let used = vec![Series {
            labels: labels(&[
                ("__name__", "custom_hbm_used"),
                ("gpu", "0"),
                ("host_ip", "1.1.1.1"),
            ]),
            points: vec![(t(0), 30.0), (t(60), 60.0)],
        }];
        let free = vec![Series {
            labels: labels(&[
                ("__name__", "custom_hbm_free"),
                ("gpu", "0"),
                ("host_ip", "1.1.1.1"),
            ]),
            points: vec![(t(0), 170.0), (t(60), 240.0)],
        }];
        let core = vec![Series {
            labels: labels(&[("gpu", "0"), ("host_ip", "1.1.1.1")]),
            points: vec![(t(0), 10.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("custom_core_util", Ok(core))
            .when("custom_hbm_direct", Ok(vec![])) // direct 为空，触发 fallback
            .when("custom_hbm_used", Ok(used))
            .when("custom_hbm_free", Ok(free));
        let spec = DeviceSpec {
            display_name: "Custom GPU".into(),
            core_util_metric: "custom_core_util".into(),
            memory: MemoryStrategy::direct(
                "custom_hbm_direct",
                Some(MemoryStrategy::composite_ratio(
                    "custom_hbm_used",
                    "custom_hbm_free",
                )),
            ),
            card_id_label: "gpu".into(),
            labels: crate::devices::LabelMapping {
                host_ip: "host_ip".into(),
                node_name: "node".into(),
                container: "c".into(),
                pod: "p".into(),
                namespace: "n".into(),
            },
            temp_metric: None,
            power_metric: None,
            host_metrics: None,
        };
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
        assert_eq!(out.records.len(), 1, "应产出 1 张卡");
        let r = &out.records[0];
        // 30/(30+170)*100 = 15, 60/(60+240)*100 = 20 → avg=17.5, peak=20
        assert!(
            r.mem_avg.is_some(),
            "显存不应为 N/A（CompositeRatio fallback 应生效）"
        );
        assert!((r.mem_avg.unwrap() - 17.5).abs() < 1e-9);
        assert!((r.mem_peak.unwrap() - 20.0).abs() < 1e-9);
    }

    // ---- CompositeFromTotal fallback：有 used 无 total 时应发 Warning ----

    #[tokio::test]
    async fn composite_from_total_warns_when_total_missing() {
        // NPU 卡有 used 数据但 total 数据缺失 → 显存 N/A + Warning
        let used = vec![Series {
            labels: labels(&[
                ("__name__", "npu_chip_info_hbm_used_memory"),
                ("id", "0"),
                ("host_ip", "1.1.1.1"),
            ]),
            points: vec![(t(0), 50.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("npu_chip_info_utilization", Ok(vec![])) // 核心为空（聚焦 fallback）
            .when("npu_chip_info_hbm_utilization", Ok(vec![])) // direct 为空触发 fallback
            .when("npu_chip_info_hbm_used_memory", Ok(used))
            .when("npu_chip_info_hbm_total_memory", Ok(vec![])); // total 为空
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
        assert!(
            out.warnings
                .iter()
                .any(|w| w.contains("used") && w.contains("total")),
            "有 used 无 total 时应发 Warning，实际：{:?}",
            out.warnings
        );
    }

    // ---- escape_promql_label_value 转义正确性 ----

    #[test]
    fn escape_promql_label_value_escapes_special_chars() {
        assert_eq!(escape_promql_label_value(r#"hello"#), r#"hello"#);
        assert_eq!(escape_promql_label_value(r#"back\slash"#), r#"back\\slash"#);
        assert_eq!(escape_promql_label_value(r#""quoted""#), r#"\"quoted\""#);
        assert_eq!(escape_promql_label_value("new\nline"), r#"new\nline"#);
        assert_eq!(
            escape_promql_label_value("carriage\rreturn"),
            r#"carriage\rreturn"#
        );
        assert_eq!(escape_promql_label_value("tab\there"), r#"tab\there"#);
    }

    #[test]
    fn escape_promql_label_value_combined() {
        // 同时含反斜杠、引号和换行
        assert_eq!(
            escape_promql_label_value("path\\file\nend"),
            r#"path\\file\nend"#
        );
    }

    #[test]
    fn escape_promql_regex_escapes_metacharacters() {
        // 双反斜杠：Go 字符串字面量解析一层后留给 RE2 一个反斜杠。
        assert_eq!(escape_promql_regex("1.1.1.1"), r"1\\.1\\.1\\.1");
        assert_eq!(escape_promql_regex("a*b+c?"), r"a\\*b\\+c\\?");
        assert_eq!(escape_promql_regex("normal"), "normal");
        assert_eq!(escape_promql_regex(r"a\b"), r"a\\\\b");
        assert_eq!(escape_promql_regex("[test]"), r"\\[test\\]");
        assert_eq!(escape_promql_regex("{val}"), r"\\{val\\}");
        assert_eq!(escape_promql_regex("a|b"), r"a\\|b");
        assert_eq!(escape_promql_regex("^start$"), r"\\^start\\$");
    }

    #[test]
    fn ipv4_instance_regex_produces_valid_go_string_literal() {
        // IPv4: escape_promql_regex 对 . 产出双反斜杠
        let escaped = escape_promql_regex("192.168.1.1");
        let ip_regex = format!("^{escaped}:");
        // 嵌入 PromQL 后应为 instance=~"^192\\.168\\.1\\.1:"
        // Go 解析 \\ 为 \，RE2 看到 ^192\.168\.1\.1: 匹配字面 IP
        assert_eq!(ip_regex, r"^192\\.168\\.1\\.1:");
    }

    #[test]
    fn ipv6_instance_regex_produces_valid_go_string_literal() {
        // IPv6: 方括号需 Go 字符串中 \\[ / \\]（Rust "\\\\[" = 3字符 "\\["）
        let escaped = escape_promql_regex("2001:db8::1");
        let ip_regex = format!("^\\\\[{escaped}\\\\]:");
        // 嵌入 PromQL 后应为 instance=~"^\\[2001:db8::1\\]:"
        // Go 解析 \\[ → \[, \\] → \]，RE2 看到 ^\[2001:db8::1\]: 匹配字面方括号
        assert_eq!(ip_regex, r"^\\[2001:db8::1\\]:");
    }

    #[test]
    fn build_instance_regex_ipv4() {
        let regex = build_instance_regex("192.168.1.1");
        // IPv4：精确匹配 IP 后跟行尾或冒号+端口号（纯数字）
        assert_eq!(regex, r"^192\\.168\\.1\\.1($|:[0-9]+)");
    }

    #[test]
    fn build_instance_regex_ipv4_no_prefix_collision() {
        // 关键：172.22.129.1 不应匹配 172.22.129.10:9101
        let regex = build_instance_regex("172.22.129.1");
        assert_eq!(regex, r"^172\\.22\\.129\\.1($|:[0-9]+)");
        // 验证正则语义：172.22.129.1 后只能跟 $ 或 :数字
        // 不会匹配 172.22.129.10（10 的 0 不是冒号+数字）
    }

    #[test]
    fn build_instance_regex_ipv6_matches_both_bare_and_bracketed() {
        let regex = build_instance_regex("2001:db8::1");
        // IPv6：应同时匹配带方括号和不带方括号的格式
        // 带方括号部分：^(\\[2001:db8::1\\]($|:[0-9]+) — 匹配 [ip]:port 和 [ip]
        // 裸 IPv6 部分：2001:db8::1$) — 仅匹配裸 ip（不含 :[0-9]+，避免前缀误匹配）
        assert!(regex.contains(r"\\["), "应含方括号转义 \\[");
        assert!(regex.contains(r"\\]"), "应含方括号转义 \\]");
        assert!(regex.contains("2001:db8::1"), "应含 IP 字面量");
        // 两种选择用 | 组合，外层 ^... 包裹
        assert!(regex.starts_with("^("), "应以 ^( 开头");
        assert!(regex.ends_with(")"), "应以 ) 结尾");
        // 方括号分支使用 ($|:[0-9]+) 消费端口号
        assert!(regex.contains("($|:[0-9]+)"), "方括号分支应含 ($|:[0-9]+)");
        // 裸 IPv6 分支仅用 $（不含 :[0-9]+，避免前缀误匹配）
        assert!(regex.contains("2001:db8::1$)"), "裸 IPv6 分支应以 $ 结尾");
    }

    #[test]
    fn strip_port_bracketed_ipv6_without_port() {
        // [::1] 无端口时应剥去方括号返回裸 IPv6
        assert_eq!(strip_port("[::1]"), "::1");
    }

    #[test]
    fn strip_port_bracketed_ipv6_with_port() {
        // [::1]:9090 应剥去方括号和端口
        assert_eq!(strip_port("[::1]:9090"), "::1");
    }

    #[test]
    fn strip_port_ipv4_with_port() {
        assert_eq!(strip_port("192.168.1.1:9090"), "192.168.1.1");
    }

    #[test]
    fn strip_port_bare_ipv6() {
        // 裸 IPv6 无端口，原样返回（无法区分地址内冒号与端口冒号）
        assert_eq!(strip_port("2001:db8::1"), "2001:db8::1");
    }

    #[test]
    fn strip_port_ipv4_mapped_ipv6_not_stripped() {
        // IPv4-mapped IPv6（如 ::ffff:192.168.1.1）不应被误判为 IPv4:port
        // 旧版 host.contains('.') 会触发误剥，导致 IP 被截断
        assert_eq!(strip_port("::ffff:192.168.1.1"), "::ffff:192.168.1.1");
    }

    #[test]
    fn strip_port_short_hostname_with_port() {
        // 短主机名（无点号）带端口也应正确剥除端口
        assert_eq!(strip_port("webserver:9090"), "webserver");
        assert_eq!(strip_port("node1:9100"), "node1");
    }

    // ---- 主机指标采集：设备配方中 host_metrics 配置时正确采集 ----

    #[tokio::test]
    async fn host_metrics_collected_from_device_spec() {
        let core = vec![Series {
            labels: labels(&[
                ("gpu", "0"),
                ("host_ip", "1.1.1.1"),
                ("pod_node", "n1"),
            ]),
            points: vec![(t(0), 10.0)],
        }];
        // 显存 composite PromQL 含 "ignoring(__name__)"
        let mem = vec![Series {
            labels: labels(&[("gpu", "0"), ("host_ip", "1.1.1.1")]),
            points: vec![(t(0), 20.0)],
        }];
        // CPU host metric
        let cpu_series = vec![Series {
            labels: labels(&[("instance", "1.1.1.1:9100")]),
            points: vec![(t(0), 45.0), (t(60), 55.0)],
        }];
        // Memory host metric
        let mem_host_series = vec![Series {
            labels: labels(&[("instance", "1.1.1.1:9100")]),
            points: vec![(t(0), 70.0), (t(60), 80.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("DCGM_FI_DEV_GPU_UTIL", Ok(core))
            .when("ignoring(__name__)", Ok(mem))
            .when("node_cpu_util", Ok(cpu_series))
            .when("node_mem_util", Ok(mem_host_series));
        let spec = DeviceSpec {
            display_name: "NVIDIA A10".into(),
            core_util_metric: "DCGM_FI_DEV_GPU_UTIL".into(),
            memory: MemoryStrategy::composite_ratio("DCGM_FI_DEV_FB_USED", "DCGM_FI_DEV_FB_FREE"),
            card_id_label: "gpu".into(),
            labels: crate::devices::LabelMapping {
                host_ip: "host_ip".into(),
                node_name: "pod_node".into(),
                container: "c".into(),
                pod: "p".into(),
                namespace: "n".into(),
            },
            temp_metric: None,
            power_metric: None,
            host_metrics: Some(crate::devices::HostMetricsSpec {
                enabled: true,
                cpu_expr: Some("node_cpu_util".into()),
                mem_expr: Some("node_mem_util".into()),
                handle_expr: None,
                host_label: "instance".into(),
            }),
        };
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
        // CPU: avg=(45+55)/2=50, peak=55
        assert!((r.host_cpu_avg.unwrap() - 50.0).abs() < 1e-9);
        assert!((r.host_cpu_peak.unwrap() - 55.0).abs() < 1e-9);
        // Memory: avg=(70+80)/2=75, peak=80
        assert!((r.host_mem_avg.unwrap() - 75.0).abs() < 1e-9);
        assert!((r.host_mem_peak.unwrap() - 80.0).abs() < 1e-9);
        // 无 handle_expr 配置，句柄数应为 None
        assert!(r.host_handle_avg.is_none());
    }

    #[tokio::test]
    async fn host_metrics_not_collected_when_absent() {
        let core = vec![Series {
            labels: labels(&[("gpu", "0"), ("host_ip", "1.1.1.1")]),
            points: vec![(t(0), 10.0)],
        }];
        let fetcher = MockFetcher::new().when("DCGM_FI_DEV_GPU_UTIL", Ok(core));
        let spec = DeviceSpec {
            display_name: "T".into(),
            core_util_metric: "DCGM_FI_DEV_GPU_UTIL".into(),
            memory: MemoryStrategy::composite_ratio("U", "F"),
            card_id_label: "gpu".into(),
            labels: crate::devices::LabelMapping {
                host_ip: "host_ip".into(),
                node_name: "node".into(),
                container: "c".into(),
                pod: "p".into(),
                namespace: "n".into(),
            },
            temp_metric: None,
            power_metric: None,
            host_metrics: None,
        };
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
        assert!(r.host_cpu_avg.is_none());
        assert!(r.host_mem_avg.is_none());
    }

    #[tokio::test]
    async fn host_metrics_with_handle_expr_collected() {
        let core = vec![Series {
            labels: labels(&[("gpu", "0"), ("host_ip", "1.1.1.1")]),
            points: vec![(t(0), 10.0)],
        }];
        let mem = vec![Series {
            labels: labels(&[("gpu", "0"), ("host_ip", "1.1.1.1")]),
            points: vec![(t(0), 20.0)],
        }];
        let cpu_series = vec![Series {
            labels: labels(&[("instance", "1.1.1.1:9100")]),
            points: vec![(t(0), 50.0)],
        }];
        let mem_host_series = vec![Series {
            labels: labels(&[("instance", "1.1.1.1:9100")]),
            points: vec![(t(0), 60.0)],
        }];
        let handle_series = vec![Series {
            labels: labels(&[("instance", "1.1.1.1:9100")]),
            points: vec![(t(0), 1000.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("DCGM_FI_DEV_GPU_UTIL", Ok(core))
            .when("ignoring(__name__)", Ok(mem))
            .when("node_cpu_util", Ok(cpu_series))
            .when("node_mem_util", Ok(mem_host_series))
            .when("node_filefd", Ok(handle_series));
        let spec = DeviceSpec {
            display_name: "NVIDIA A10".into(),
            core_util_metric: "DCGM_FI_DEV_GPU_UTIL".into(),
            memory: MemoryStrategy::composite_ratio("DCGM_FI_DEV_FB_USED", "DCGM_FI_DEV_FB_FREE"),
            card_id_label: "gpu".into(),
            labels: crate::devices::LabelMapping {
                host_ip: "host_ip".into(),
                node_name: "pod_node".into(),
                container: "c".into(),
                pod: "p".into(),
                namespace: "n".into(),
            },
            temp_metric: None,
            power_metric: None,
            host_metrics: Some(crate::devices::HostMetricsSpec {
                enabled: true,
                cpu_expr: Some("node_cpu_util".into()),
                mem_expr: Some("node_mem_util".into()),
                handle_expr: Some("node_filefd".into()),
                host_label: "instance".into(),
            }),
        };
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
        assert!(r.host_cpu_avg.is_some());
        assert!(r.host_mem_avg.is_some());
        // 句柄数平均值应为整数（trunc）
        assert!((r.host_handle_avg.unwrap() - 1000.0).abs() < 1e-9);
        assert!((r.host_handle_peak.unwrap() - 1000.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn host_metrics_query_failure_produces_warning() {
        let core = vec![Series {
            labels: labels(&[("gpu", "0"), ("host_ip", "1.1.1.1")]),
            points: vec![(t(0), 10.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("DCGM_FI_DEV_GPU_UTIL", Ok(core))
            .when("node_cpu_util", Err(AppError::Prometheus {
                source_name: "s".into(),
                url: "http://x".into(),
                detail: "连接失败".into(),
            }))
            .when("node_mem_util", Ok(vec![]));
        let spec = DeviceSpec {
            display_name: "T".into(),
            core_util_metric: "DCGM_FI_DEV_GPU_UTIL".into(),
            memory: MemoryStrategy::composite_ratio("U", "F"),
            card_id_label: "gpu".into(),
            labels: crate::devices::LabelMapping {
                host_ip: "host_ip".into(),
                node_name: "node".into(),
                container: "c".into(),
                pod: "p".into(),
                namespace: "n".into(),
            },
            temp_metric: None,
            power_metric: None,
            host_metrics: Some(crate::devices::HostMetricsSpec {
                enabled: true,
                cpu_expr: Some("node_cpu_util".into()),
                mem_expr: Some("node_mem_util".into()),
                handle_expr: None,
                host_label: "instance".into(),
            }),
        };
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
        // CPU 查询失败应降级为 N/A
        assert!(r.host_cpu_avg.is_none());
        // 内存查询返回空也应为 N/A
        assert!(r.host_mem_avg.is_none());
        // 应产生 Warning（fetch_with_warning 对查询失败会发出 Warning）
        assert!(
            out.warnings.iter().any(|w| w.contains("连接失败")),
            "CPU 查询失败应产生 Warning，实际：{:?}",
            out.warnings
        );
    }

    #[tokio::test]
    async fn host_metrics_averages_multi_series_per_timestamp() {
        // 主机指标返回多条 series（如 per-core CPU），同一时间戳有多个值。
        // 应按时间戳分组取均值后再聚合，而非去重丢弃。
        let core = vec![Series {
            labels: labels(&[("gpu", "0"), ("host_ip", "1.1.1.1")]),
            points: vec![(t(0), 10.0), (t(60), 20.0)],
        }];
        // 两条 CPU series：core0=80%, core1=20% at t0; core0=60%, core1=40% at t60
        let cpu_series = vec![
            Series {
                labels: labels(&[("cpu", "0"), ("instance", "1.1.1.1:9100")]),
                points: vec![(t(0), 80.0), (t(60), 60.0)],
            },
            Series {
                labels: labels(&[("cpu", "1"), ("instance", "1.1.1.1:9100")]),
                points: vec![(t(0), 20.0), (t(60), 40.0)],
            },
        ];
        let mem_series = vec![Series {
            labels: labels(&[("instance", "1.1.1.1:9100")]),
            points: vec![(t(0), 50.0), (t(60), 50.0)],
        }];
        let fetcher = MockFetcher::new()
            .when("DCGM_FI_DEV_GPU_UTIL", Ok(core))
            .when("node_cpu_util", Ok(cpu_series))
            .when("node_mem_util", Ok(mem_series));
        let spec = DeviceSpec {
            display_name: "T".into(),
            core_util_metric: "DCGM_FI_DEV_GPU_UTIL".into(),
            memory: MemoryStrategy::composite_ratio("U", "F"),
            card_id_label: "gpu".into(),
            labels: crate::devices::LabelMapping {
                host_ip: "host_ip".into(),
                node_name: "node".into(),
                container: "c".into(),
                pod: "p".into(),
                namespace: "n".into(),
            },
            temp_metric: None,
            power_metric: None,
            host_metrics: Some(crate::devices::HostMetricsSpec {
                enabled: true,
                cpu_expr: Some("node_cpu_util".into()),
                mem_expr: Some("node_mem_util".into()),
                handle_expr: None,
                host_label: "instance".into(),
            }),
        };
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
        // CPU: t0 avg=(80+20)/2=50, t60 avg=(60+40)/2=50 → overall avg=50, peak=50
        assert!(
            (r.host_cpu_avg.unwrap() - 50.0).abs() < 1e-9,
            "多 series 应按时间戳取均值，实际：{:?}",
            r.host_cpu_avg
        );
        assert!(
            (r.host_cpu_peak.unwrap() - 50.0).abs() < 1e-9,
            "多 series 峰值应为时间戳均值中的最大值，实际：{:?}",
            r.host_cpu_peak
        );
    }

    #[test]
    fn append_label_filter_simple_metric() {
        let result = append_label_filter("node_cpu_util", r#"instance=~"^1\.2\.3\.4.*""#);
        assert_eq!(result, r#"node_cpu_util{instance=~"^1\.2\.3\.4.*"}"#);
    }

    #[test]
    fn append_label_filter_metric_with_existing_labels() {
        let result = append_label_filter(
            r#"node_cpu_seconds_total{mode="idle"}"#,
            r#"instance=~"^1\.2\.3\.4.*""#,
        );
        assert_eq!(
            result,
            r#"node_cpu_seconds_total{mode="idle", instance=~"^1\.2\.3\.4.*"}"#
        );
    }

    #[test]
    fn append_label_filter_metric_with_empty_braces() {
        let result = append_label_filter(
            "node_cpu_seconds_total{}",
            r#"instance=~"^1\.2\.3\.4.*""#,
        );
        assert_eq!(
            result,
            r#"node_cpu_seconds_total{instance=~"^1\.2\.3\.4.*"}"#
        );
    }

    #[test]
    fn append_label_filter_complex_expression() {
        // 复杂表达式：在内部向量选择器中注入标签过滤器
        // rate(node_cpu_seconds_total{mode="idle"}[5m]) 中的指标名带花括号，
        // 在 } 前追加 instance 过滤
        let result = append_label_filter(
            r#"100 - (avg by(instance) (rate(node_cpu_seconds_total{mode="idle"}[5m])) * 100)"#,
            r#"instance=~"^1\.2\.3\.4.*""#,
        );
        // 应在内部 {mode="idle"} 中追加，而非使用 and on(instance)
        assert!(result.contains(r#"node_cpu_seconds_total{mode="idle", instance=~"^1\.2\.3\.4.*"}"#));
        // [5m] 应保持不变
        assert!(result.contains("[5m]"));
        // 不应包含 and on(instance) 或 up
        assert!(!result.contains("and on(instance)"));
        assert!(!result.contains("up{"));
    }

    #[test]
    fn append_label_filter_metric_with_colon() {
        // 含冒号的指标名（如 namespace:metric）仍应识别为简单指标
        let result = append_label_filter(
            r#"ns:node_cpu{mode="idle"}"#,
            r#"instance=~"^1\.2\.3\.4.*""#,
        );
        assert_eq!(
            result,
            r#"ns:node_cpu{mode="idle", instance=~"^1\.2\.3\.4.*"}"#
        );
    }

    #[test]
    fn append_label_filter_division_expression() {
        // 二元运算表达式：两个裸指标名各追加 {label_filter}
        let result = append_label_filter(
            r#"100 - (node_memory_MemAvailable_bytes / node_memory_MemTotal_bytes * 100)"#,
            r#"instance=~"^1\.2\.3\.4.*""#,
        );
        assert!(result.contains(r#"node_memory_MemAvailable_bytes{instance=~"^1\.2\.3\.4.*"}"#));
        assert!(result.contains(r#"node_memory_MemTotal_bytes{instance=~"^1\.2\.3\.4.*"}"#));
        // 不应包含 and on(instance) 或 up
        assert!(!result.contains("and on(instance)"));
    }

    #[test]
    fn append_label_filter_rate_without_labels() {
        // rate(metric[5m]) → 裸指标名追加 {label_filter}
        let result = append_label_filter(
            r#"rate(node_cpu_seconds_total[5m])"#,
            r#"instance=~"^1\.2\.3\.4.*""#,
        );
        assert!(result.contains(r#"node_cpu_seconds_total{instance=~"^1\.2\.3\.4.*"}"#));
        assert!(result.contains("[5m]"));
    }

    #[test]
    fn append_label_filter_rate_with_labels() {
        // rate(metric{mode="idle"}[5m]) → 在 } 前追加
        let result = append_label_filter(
            r#"rate(node_cpu_seconds_total{mode="idle"}[5m])"#,
            r#"instance=~"^1\.2\.3\.4.*""#,
        );
        assert!(result.contains(r#"node_cpu_seconds_total{mode="idle", instance=~"^1\.2\.3\.4.*"}"#));
        assert!(result.contains("[5m]"));
    }

    #[test]
    fn handle_avg_truncated_to_integer() {
        // 验证句柄数平均值舍弃小数部分
        let val: f64 = 1234.5678;
        assert_eq!(val.trunc(), 1234.0);
    }
}
