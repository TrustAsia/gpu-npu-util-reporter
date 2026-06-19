//! GPU/NPU 多源利用率监控 CLI 入口。
//!
//! 编排流水线：config → fetcher → processor → mapper → highlight → reporter。
//! 单源/单卡失败降级为 N/A，仅打印警告；致命错误打印中文提示并退出码 1。

mod config;
mod devices;
mod error;
mod fetcher;
mod highlight;
mod mapper;
mod processor;
mod reporter;

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use clap::Parser;
use config::{AppConfig, CliOverrides};
use devices::{DeviceSpec, MemoryStrategy};
use error::AppError;
use fetcher::{MetricFetcher, PrometheusFetcher};
use processor::{aggregate, CardRecord, Series};
use std::collections::HashMap;
use std::process::ExitCode;

/// CLI 参数。
#[derive(Parser, Debug)]
#[command(name = "gpu-util-monitor", about = "GPU/NPU 利用率监控与报表生成")]
struct Args {
    /// 配置文件路径（不存在则生成默认并退出）。
    #[arg(long, default_value = "./config.yaml")]
    config: String,
    /// 覆盖起始时间 YYYY-MM-DD HH:MM:SS（须与 --end 同时给）。
    #[arg(long)]
    start: Option<String>,
    /// 覆盖结束时间。
    #[arg(long)]
    end: Option<String>,
    /// 覆盖输出路径。
    #[arg(long)]
    output: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let overrides = CliOverrides {
        start: args.start.clone(),
        end: args.end.clone(),
        config_path: Some(args.config.clone()),
        output: args.output.clone(),
    };

    // 1. 加载配置
    let cfg = match config::load_or_init(&args.config) {
        Ok(None) => {
            println!(
                "[提示] 未发现配置文件，已在 {} 生成默认配置，请编辑后重新运行。",
                args.config
            );
            return ExitCode::SUCCESS;
        }
        Ok(Some(c)) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let cfg = match config::apply_overrides(cfg, &overrides) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    // 2. 解析时间范围
    let start = match parse_time(&cfg.time_range.start) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let end = match parse_time(&cfg.time_range.end) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    let step = Duration::seconds(cfg.report.query_step_secs as i64);

    // 3. 采集 + 聚合
    let mut warnings: Vec<String> = Vec::new();
    let mut records: Vec<CardRecord> = Vec::new();
    for src in &cfg.sources {
        let fetcher =
            PrometheusFetcher::new(src.name.clone(), src.url.clone(), src.timeout_secs);
        for dt_key in &src.device_types {
            let spec = match cfg.devices.get(dt_key) {
                Some(s) => s.clone(),
                None => {
                    warnings.push(format!(
                        "数据源 {} 引用了未定义的设备类型 {}",
                        src.name, dt_key
                    ));
                    continue;
                }
            };
            match collect_device(&fetcher, &src.name, &spec, start, end, step, &cfg).await {
                Ok(mut recs) => records.append(&mut recs),
                Err(e) => warnings.push(format!("{e}")),
            }
        }
    }

    // 4. 资产映射（可选）
    let mut mapping_values: HashMap<(usize, String), String> = HashMap::new();
    let mapping_columns: Vec<mapper::MappingColumn> = if let Some(m) = &cfg.mapping {
        if m.enabled {
            match mapper::load_asset_table(&m.source_path, &m.match_keys) {
                Ok(assets) => {
                    for (i, rec) in records.iter().enumerate() {
                        let joined = mapper::join_record(rec, &assets, m);
                        for (rename, val) in joined {
                            mapping_values.insert((i, rename), val);
                        }
                    }
                    m.columns.clone()
                }
                Err(e) => {
                    warnings.push(format!("{e}"));
                    vec![]
                }
            }
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    // 5. 渲染
    let spec = reporter::ReportSpec {
        base_columns: mapper::BASE_COLUMNS.iter().map(|s| s.to_string()).collect(),
        mapping_renames: mapping_columns.iter().map(|c| c.rename.clone()).collect(),
    };
    match reporter::render_to_buffer(
        &records,
        &spec,
        &mapping_columns,
        &cfg.thresholds,
        &mapping_values,
    ) {
        Ok(buf) => {
            if let Err(e) = std::fs::write(&cfg.report.output_path, buf) {
                eprintln!("[错误] 报表写入失败：{e}");
                return ExitCode::from(1);
            }
            println!("[完成] 报表已生成：{}", cfg.report.output_path);
        }
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    }

    for w in &warnings {
        eprintln!("{w}");
    }
    ExitCode::SUCCESS
}

/// 采集一个设备类型在一个源上的所有卡，聚合成 CardRecord 列表。
async fn collect_device(
    fetcher: &dyn MetricFetcher,
    source_name: &str,
    spec: &DeviceSpec,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step: Duration,
    cfg: &AppConfig,
) -> Result<Vec<CardRecord>, AppError> {
    // 核心利用率
    let core_series = fetcher
        .query_range(&spec.core_util_metric, start, end, step)
        .await
        .unwrap_or_default();

    // 显存：根据策略
    let mem_series: Vec<Series> = match &spec.memory {
        MemoryStrategy::CompositeRatio(b) => {
            let q = fetcher::gpu_memory_promql(&b.composite_ratio.used, &b.composite_ratio.free);
            fetcher.query_range(&q, start, end, step).await.unwrap_or_default()
        }
        MemoryStrategy::DirectMetric(b) => {
            fetcher.query_range(&b.direct_metric.metric, start, end, step).await.unwrap_or_default()
        }
        MemoryStrategy::CompositeFromTotal(_) => vec![],
    };

    // NPU fallback：direct 为空时拉 used/total
    let mut effective_mem = mem_series.clone();
    if effective_mem.is_empty() {
        if let MemoryStrategy::DirectMetric(b) = &spec.memory {
            if let Some(fb) = &b.direct_metric.fallback {
                if let MemoryStrategy::CompositeFromTotal(body) = fb.as_ref() {
                    let used = &body.composite_from_total.used;
                    let total = &body.composite_from_total.total;
                    let used_s = fetcher.query_range(used, start, end, step).await.unwrap_or_default();
                    let total_s = fetcher.query_range(total, start, end, step).await.unwrap_or_default();
                    effective_mem = used_s
                        .iter()
                        .map(|u| {
                            processor::hbm_fallback_series(
                                u,
                                &total_s
                                    .iter()
                                    .find(|t| t.labels == u.labels)
                                    .cloned()
                                    .unwrap_or_default(),
                            )
                        })
                        .collect();
                }
            }
        }
    }

    // 按 (host_ip, card_id) 分组聚合。
    // 分组值：(core_series, mem_series)。先插入空默认值，再 move 实际序列，
    // 避免 or_insert_with 闭包捕获 s 导致的 move 冲突。
    let mut groups: HashMap<String, (Series, Option<Series>)> = HashMap::new();
    for s in core_series {
        let key = series_key(&s, spec, cfg);
        groups.entry(key).or_default().0 = s;
    }
    for s in effective_mem {
        let key = series_key(&s, spec, cfg);
        groups.entry(key).or_default().1 = Some(s);
    }

    let mut out = Vec::new();
    for (_, (core, mem)) in groups {
        let host_ip = extract_ip(&core.labels, &cfg.host_ip.prefer_label);
        let card_id = core
            .labels
            .get(&spec.card_id_label)
            .cloned()
            .unwrap_or_default();
        let node_name = core.labels.get("node").cloned().unwrap_or_default();
        let (c_avg, c_peak, c_peak_t) = stat3(&core.points);
        let (m_avg, m_peak, m_peak_t) = mem
            .as_ref()
            .map(|m| stat3(&m.points))
            .unwrap_or((None, None, None));

        // 归属（末态简化：取标签瞬时值；完整 last_in_range 见 processor::last_non_empty）
        let namespace = core
            .labels
            .get(&spec.labels.namespace)
            .cloned()
            .unwrap_or_default();
        let pod = core.labels.get(&spec.labels.pod).cloned().unwrap_or_default();
        let container = core
            .labels
            .get(&spec.labels.container)
            .cloned()
            .unwrap_or_default();

        out.push(CardRecord {
            source_name: source_name.into(),
            host_ip,
            node_name,
            card_id,
            device_type: spec.display_name.clone(),
            namespace,
            pod,
            container,
            core_avg: c_avg,
            core_peak: c_peak,
            core_peak_time: c_peak_t,
            mem_avg: m_avg,
            mem_peak: m_peak,
            mem_peak_time: m_peak_t,
            range_start: start,
            range_end: end,
        });
    }
    Ok(out)
}

/// 把一组点聚合成 (avg, peak, peak_time)，空则全 None。
fn stat3(
    points: &[(DateTime<Utc>, f64)],
) -> (Option<f64>, Option<f64>, Option<DateTime<Utc>>) {
    match aggregate(points) {
        Some(s) => (Some(s.avg), Some(s.peak), Some(s.peak_time)),
        None => (None, None, None),
    }
}

/// 序列分组 key：host_ip + card_id。
fn series_key(s: &Series, spec: &DeviceSpec, cfg: &AppConfig) -> String {
    let ip = extract_ip(&s.labels, &cfg.host_ip.prefer_label);
    let card = s
        .labels
        .get(&spec.card_id_label)
        .cloned()
        .unwrap_or_default();
    format!("{ip}|{card}")
}

/// 从标签取主机 IP：优先 prefer_label，否则 instance 去端口。
fn extract_ip(labels: &HashMap<String, String>, prefer: &str) -> String {
    if let Some(v) = labels.get(prefer) {
        if !v.is_empty() {
            return v.clone();
        }
    }
    labels
        .get("instance")
        .map(|s| {
            s.rsplit_once(':')
                .map(|(h, _)| h.to_string())
                .unwrap_or_else(|| s.clone())
        })
        .unwrap_or_default()
}

fn parse_time(s: &str) -> Result<DateTime<Utc>, AppError> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .map(|ndt| DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
        .map_err(|_| AppError::TimeFormat { raw: s.into() })
}
