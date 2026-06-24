//! GPU/NPU 多源利用率监控 CLI 入口。
//!
//! 仅负责参数解析与顶层流程串联；采集/聚合/归属/分组逻辑统一下沉到
//! [`gpu_npu_util_reporter::pipeline`]，便于集成测试覆盖。单源/单卡失败
//! 降级为 N/A 并收集 Warning（PRD §5.2），致命错误打印中文提示并退出码 1。

use gpu_npu_util_reporter::config;
use gpu_npu_util_reporter::error::AppError;
use gpu_npu_util_reporter::fetcher::{MetricFetcher, PrometheusFetcher};
use gpu_npu_util_reporter::logging;
use gpu_npu_util_reporter::mapper;
use gpu_npu_util_reporter::mapper::ColumnFlags;
use gpu_npu_util_reporter::pipeline;
use gpu_npu_util_reporter::processor::CardRecord;
use gpu_npu_util_reporter::reporter;
use gpu_npu_util_reporter::template;
use gpu_npu_util_reporter::time_expr;

use chrono::{DateTime, Duration, Utc};
use clap::Parser;
use config::CliOverrides;
use std::collections::HashMap;
use std::process::ExitCode;
use tracing::{error, info, warn};

/// CLI 参数。
#[derive(Parser, Debug)]
#[command(name = "gpu-npu-util-reporter", about = "GPU/NPU 利用率监控与报表生成")]
struct Args {
    /// 配置文件路径（不存在则生成默认并退出）。
    #[arg(long, default_value = "./config.yaml")]
    config: String,
    /// 覆盖起始时间（绝对: YYYY-MM-DD HH:MM:SS 或相对: now-7d, start+3h 等）
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
#[allow(clippy::too_many_lines)]
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

    // 2. 解析时间范围（支持相对时间表达式）
    //    两遍解析：先解析不含 start/end 依赖的表达式（如 now-7d），
    //    再用已解析的 start/end 解析依赖它们的表达式（如 end-1d）。
    //    注意：时区需在此前解析，因为绝对时间（如 "00:00:01"）
    //    需按配置时区解释为本地时间后转 UTC。
    let now = Utc::now();
    // 时区已在 validate_config 中校验，此处必然成功。
    let tz: chrono_tz::Tz = cfg
        .timezone
        .parse()
        .expect("timezone already validated in config");
    // 第一遍：尝试解析 start（仅 now 上下文）
    let start = if let Ok(t) = resolve_time(&cfg.time_range.start, now, None, None, tz) {
        t
    } else {
        // start 可能引用 end（如 "end-1d"），先解析 end 再重试
        let end = match resolve_time(&cfg.time_range.end, now, None, None, tz) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(1);
            }
        };
        match resolve_time(&cfg.time_range.start, now, None, Some(end), tz) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(1);
            }
        }
    };
    // 第二遍：用已解析的 start 解析 end
    let end = match resolve_time(&cfg.time_range.end, now, Some(start), None, tz) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };
    if start >= end {
        eprintln!("[错误] --start 必须早于 --end（start={start}, end={end}）");
        return ExitCode::from(1);
    }

    // 3. 渲染模板变量（路径中的 {{start}}, {{end}}, {{now}} 等）
    let tpl_ctx = template::TemplateContext {
        start,
        end,
        now,
        tz,
    };
    let output_path = template::render_template(&cfg.report.output_path, &tpl_ctx);
    let log_file_path = template::render_template(&cfg.log.file_path, &tpl_ctx);

    // 4. 初始化日志（需在模板渲染后，因为日志路径可能含模板变量）
    let log_cfg = config::LogConfig {
        console_level: cfg.log.console_level.clone(),
        file_enabled: cfg.log.file_enabled,
        file_level: cfg.log.file_level.clone(),
        file_path: log_file_path.clone(),
    };
    let _log_guard = logging::init_logging(&log_cfg);

    info!("配置加载完成");
    let start_local = start.with_timezone(&tz);
    let end_local = end.with_timezone(&tz);
    info!(
        "时间范围：{} ~ {}",
        start_local.format("%Y-%m-%d %H:%M:%S"),
        end_local.format("%Y-%m-%d %H:%M:%S")
    );
    info!("报表输出路径：{output_path}");
    if cfg.log.file_enabled {
        info!("日志文件路径：{log_file_path}");
    }

    let step =
        Duration::try_seconds(cfg.report.query_step_secs.cast_signed()).unwrap_or_else(|| {
            error!(
                "query_step_secs 过大（{}），使用默认 60 秒",
                cfg.report.query_step_secs
            );
            Duration::seconds(60)
        });

    // 5. 采集 + 聚合（单源/单卡失败 → Warning，不中断）
    let mut warnings: Vec<String> = Vec::new();
    let mut records: Vec<CardRecord> = Vec::new();
    for src in &cfg.sources {
        info!(
            "开始采集数据源「{}」（{}）",
            src.name,
            AppError::redact_url(&src.url)
        );
        let fetcher = PrometheusFetcher::new(src.name.clone(), src.url.clone(), src.timeout_secs);
        for dt_key in &src.device_types {
            let spec = if let Some(s) = cfg.devices.get(dt_key) {
                s.clone()
            } else {
                let msg = format!("数据源 {} 引用了未定义的设备类型 {}", src.name, dt_key);
                warn!("{msg}");
                warnings.push(msg);
                continue;
            };
            info!("  采集设备类型「{}」({})", spec.display_name, dt_key);
            let outcome =
                pipeline::collect_device(&fetcher, &src.name, &spec, start, end, step, &cfg).await;
            for w in &outcome.warnings {
                warn!("{w}");
            }
            warnings.extend(outcome.warnings);
            info!(
                "  设备类型「{}」采集完成：{} 条记录",
                spec.display_name,
                outcome.records.len()
            );
            records.extend(outcome.records);
        }
        info!("数据源「{}」采集完成", src.name);
    }
    info!("全部采集完成，共 {} 条记录", records.len());

    // 5.5. 主机指标采集（可选，通用指标）
    let mut column_flags = ColumnFlags::default();
    // 检查是否有设备配置了温度/功率指标（仅检查被数据源实际引用的设备类型，
    // 避免未引用的设备类型在报表中产生全 N/A 的额外列）
    let active_device_keys: std::collections::HashSet<&String> =
        cfg.sources.iter().flat_map(|s| &s.device_types).collect();
    for (key, spec) in &cfg.devices {
        if active_device_keys.contains(key) {
            if spec.temp_metric.is_some() {
                column_flags.has_temp = true;
            }
            if spec.power_metric.is_some() {
                column_flags.has_power = true;
            }
        }
    }
    if let Some(hm) = &cfg.host_metrics {
        if hm.enabled {
            column_flags.has_host_cpu = true;
            column_flags.has_host_mem = true;
            column_flags.has_host_handle = hm.handle_metric.is_some();

            // 确定使用哪个数据源
            let host_source = if let Some(ref name) = hm.source {
                cfg.sources.iter().find(|s| &s.name == name)
            } else {
                cfg.sources.first()
            };
            if let Some(host_src) = host_source {
                info!("开始采集主机指标（数据源「{}」）", host_src.name);
                let host_fetcher = PrometheusFetcher::new(
                    host_src.name.clone(),
                    host_src.url.clone(),
                    host_src.timeout_secs,
                );

                // 收集唯一的主机 IP
                let host_ips: Vec<String> = records
                    .iter()
                    .map(|r| r.host_ip.clone())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();

                for ip in &host_ips {
                    if ip.is_empty() {
                        continue;
                    }
                    let escaped_ip = pipeline::escape_promql_regex(ip);
                    // 精确匹配主机 IP，避免前缀碰撞（如 "192.168.1.10" 不应匹配 "192.168.1.100"）。
                    // host_label 值可能为裸 IP 或 "ip:port" 格式，用 ($|:) 精确锚定边界。
                    let ip_regex = if ip.contains(':') {
                        // IPv6：host_label 值可能为 [ip]:port，正则需匹配字面方括号。
                        // Go 字符串字面量中 \\\\ 解析为 \\，RE2 将 \\[ 视为字面 [。
                        // Rust "\\\\[" = 3字符 "\\["，嵌入 PromQL 后 Go 解析为 "\["，RE2 匹配字面 [。
                        format!("^\\\\[{escaped_ip}\\\\]($|:)")
                    } else {
                        format!("^{escaped_ip}($|:)")
                    };
                    let label_filter = format!("{}=~\"{}\"", hm.host_label, ip_regex);

                    // 查询 CPU 利用率
                    let cpu_promql = format!("{}{{{}}}", hm.cpu_metric, label_filter);
                    let (cpu_avg, cpu_peak, cpu_peak_t) = query_host_metric(
                        &host_fetcher, &cpu_promql, start, end, step, ip, "CPU",
                    )
                    .await;

                    // 查询内存利用率
                    let mem_promql = format!("{}{{{}}}", hm.mem_metric, label_filter);
                    let (mem_avg, mem_peak, mem_peak_t) = query_host_metric(
                        &host_fetcher, &mem_promql, start, end, step, ip, "内存",
                    )
                    .await;

                    // 查询句柄数（可选）
                    let (handle_avg, handle_peak, handle_peak_t) =
                        if let Some(handle_metric) = &hm.handle_metric {
                            let h_promql = format!("{handle_metric}{{{}}}", label_filter);
                            query_host_metric(
                                &host_fetcher, &h_promql, start, end, step, ip, "句柄数",
                            )
                            .await
                        } else {
                            (None, None, None)
                        };

                    // 填入该主机下所有卡记录
                    for rec in &mut records {
                        if rec.host_ip == *ip {
                            rec.host_cpu_avg = cpu_avg;
                            rec.host_cpu_peak = cpu_peak;
                            rec.host_cpu_peak_time = cpu_peak_t;
                            rec.host_mem_avg = mem_avg;
                            rec.host_mem_peak = mem_peak;
                            rec.host_mem_peak_time = mem_peak_t;
                            rec.host_handle_avg = handle_avg;
                            rec.host_handle_peak = handle_peak;
                            rec.host_handle_peak_time = handle_peak_t;
                        }
                    }
                }
                info!("主机指标采集完成（{} 台主机）", host_ips.len());
            } else {
                warn!("主机指标已启用但未找到匹配的数据源，跳过主机指标采集");
            }
        }
    }

    // 构建动态基础列（含可选指标组）
    let base_columns = mapper::build_base_columns(column_flags);
    let base_column_refs: Vec<&str> = base_columns.iter().map(String::as_str).collect();

    // 6. 渲染前稳定排序（I1）：必须在资产映射之前，保证 mapping_values 的行索引
    //    与最终输出顺序一致。按 (source_name, host_ip, card_id) 升序。
    records.sort_by(|a, b| {
        a.source_name
            .cmp(&b.source_name)
            .then(a.host_ip.cmp(&b.host_ip))
            .then(a.card_id.cmp(&b.card_id))
    });

    // 7. 资产映射（可选，支持多来源）
    info!("开始资产映射");
    let mut mapping_values: HashMap<(usize, String), String> = HashMap::new();
    let mapping_columns: Vec<mapper::MappingColumn> = if let Some(m) = &cfg.mapping {
        if m.enabled {
            let all_cols = m.all_columns_owned();
            for src in &m.sources {
                info!("加载资产表：{}", src.source_path);
                match mapper::load_asset_table(
                    &src.source_path,
                    &src.match_keys,
                    src.source_sheet.as_deref(),
                ) {
                    Ok(assets) => {
                        info!("资产表加载完成：{} 行", assets.len());
                        let (index, dup_warnings) = mapper::build_asset_index(&assets);
                        for w in &dup_warnings {
                            warn!("{w}");
                        }
                        warnings.extend(dup_warnings);
                        let mut joined_count = 0usize;
                        for (i, rec) in records.iter().enumerate() {
                            let joined = mapper::join_record(rec, &index, src);
                            if !joined.is_empty() {
                                joined_count += 1;
                            }
                            for (rename, val) in joined {
                                mapping_values.insert((i, rename), val);
                            }
                        }
                        info!(
                            "资产映射完成（{}）：{joined_count}/{} 行命中",
                            src.source_path,
                            records.len()
                        );
                    }
                    Err(e) => {
                        warn!("{e}");
                        warnings.push(format!("{e}"));
                    }
                }
            }
            // PRD §2.3：缺失锚点（非基础列）应记 Warning。
            warnings.extend(mapper::missing_anchor_warnings(
                &base_column_refs,
                &all_cols,
            ));
            all_cols
        } else {
            info!("资产映射已关闭");
            vec![]
        }
    } else {
        info!("未配置资产映射");
        vec![]
    };

    // 8. 渲染
    info!("开始渲染报表");
    let spec = reporter::ReportSpec {
        base_columns: base_columns.clone(),
        mapping_renames: mapping_columns.iter().map(|c| c.rename.clone()).collect(),
    };
    match reporter::render_to_buffer(
        &records,
        &spec,
        &mapping_columns,
        &cfg.thresholds,
        &mapping_values,
        tz,
    ) {
        Ok(buf) => {
            // 创建输出目录（如果路径含模板变量如 {{start_date}}，目录可能不存在）
            if let Some(parent) = std::path::Path::new(&output_path).parent() {
                if !parent.as_os_str().is_empty() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        error!("输出目录创建失败：{e}（将继续尝试写入）");
                    }
                }
            }
            if let Err(e) = std::fs::write(&output_path, buf) {
                error!("报表写入失败：{e}");
                return ExitCode::from(1);
            }
            info!(
                "报表已生成：{output_path}（{} 条记录，{} 字节）",
                records.len(),
                std::fs::metadata(&output_path).map_or(0, |m| m.len())
            );
        }
        Err(e) => {
            error!("{e}");
            return ExitCode::from(1);
        }
    }

    // 9. 数据库推送（可选）
    if let Some(db_cfg) = &cfg.database {
        if db_cfg.enabled {
            info!("开始推送数据到 MySQL");
            match gpu_npu_util_reporter::db::push_to_database(
                &records,
                db_cfg,
                &mapping_values,
                &base_columns,
                &mapping_columns,
                tz,
            )
            .await
            {
                Ok(()) => info!("数据库推送完成"),
                Err(e) => {
                    error!("{e}");
                    return ExitCode::from(1);
                }
            }
        }
    }

    if !warnings.is_empty() {
        warn!("共 {} 条警告", warnings.len());
        for w in &warnings {
            warn!("{w}");
        }
    }
    info!("运行完成");
    ExitCode::SUCCESS
}

/// 查询主机指标（CPU/内存/句柄数），合并所有 series 的点后聚合为 (avg, peak, peak_time)。
async fn query_host_metric(
    fetcher: &PrometheusFetcher,
    promql: &str,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step: Duration,
    ip: &str,
    label: &str,
) -> (Option<f64>, Option<f64>, Option<DateTime<Utc>>) {
    match fetcher.query_range(promql, start, end, step).await {
        Ok(series) => {
            let mut all_points: Vec<(DateTime<Utc>, f64)> =
                series.iter().flat_map(|s| s.points.clone()).collect();
            // 与 pipeline::merge_points_into 语义一致：同一时间戳保留最后一个值。
            all_points.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
            all_points.reverse();
            all_points.dedup_by(|a, b| a.0 == b.0);
            all_points.reverse();
            gpu_npu_util_reporter::processor::aggregate(&all_points).map_or(
                (None, None, None),
                |s| (Some(s.avg), Some(s.peak), Some(s.peak_time)),
            )
        }
        Err(e) => {
            warn!("主机 {ip} {label} 指标查询失败：{e}");
            (None, None, None)
        }
    }
}

/// 解析时间字符串（支持绝对时间和相对时间表达式）。
fn resolve_time(
    s: &str,
    now: DateTime<Utc>,
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    tz: chrono_tz::Tz,
) -> Result<DateTime<Utc>, AppError> {
    let ctx = time_expr::TimeContext {
        now,
        start,
        end,
        tz,
    };
    time_expr::resolve_time_expr(s, &ctx)
}
