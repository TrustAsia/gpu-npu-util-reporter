//! GPU/NPU 多源利用率监控 CLI 入口。
//!
//! 仅负责参数解析与顶层流程串联；采集/聚合/归属/分组逻辑统一下沉到
//! [`gpu_npu_util_reporter::pipeline`]，便于集成测试覆盖。单源/单卡失败
//! 降级为 N/A 并收集 Warning（PRD §5.2），致命错误打印中文提示并退出码 1。

use gpu_npu_util_reporter::config;
use gpu_npu_util_reporter::error::AppError;
use gpu_npu_util_reporter::fetcher::PrometheusFetcher;
use gpu_npu_util_reporter::mapper;
use gpu_npu_util_reporter::pipeline;
use gpu_npu_util_reporter::processor::CardRecord;
use gpu_npu_util_reporter::reporter;

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use clap::Parser;
use config::CliOverrides;
use std::collections::HashMap;
use std::process::ExitCode;

/// CLI 参数。
#[derive(Parser, Debug)]
#[command(name = "gpu-npu-util-reporter", about = "GPU/NPU 利用率监控与报表生成")]
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
    let step = Duration::seconds(cfg.report.query_step_secs.cast_signed());

    // 3. 采集 + 聚合（单源/单卡失败 → Warning，不中断）
    let mut warnings: Vec<String> = Vec::new();
    let mut records: Vec<CardRecord> = Vec::new();
    for src in &cfg.sources {
        let fetcher = PrometheusFetcher::new(src.name.clone(), src.url.clone(), src.timeout_secs);
        for dt_key in &src.device_types {
            let spec = if let Some(s) = cfg.devices.get(dt_key) {
                s.clone()
            } else {
                warnings.push(format!(
                    "数据源 {} 引用了未定义的设备类型 {}",
                    src.name, dt_key
                ));
                continue;
            };
            let outcome =
                pipeline::collect_device(&fetcher, &src.name, &spec, start, end, step, &cfg).await;
            warnings.extend(outcome.warnings);
            records.extend(outcome.records);
        }
    }

    // 4. 渲染前稳定排序（I1）：必须在资产映射之前，保证 mapping_values 的行索引
    //    与最终输出顺序一致。按 (source_name, host_ip, card_id) 升序。
    records.sort_by(|a, b| {
        a.source_name
            .cmp(&b.source_name)
            .then(a.host_ip.cmp(&b.host_ip))
            .then(a.card_id.cmp(&b.card_id))
    });

    // 5. 资产映射（可选）
    let mut mapping_values: HashMap<(usize, String), String> = HashMap::new();
    let mapping_columns: Vec<mapper::MappingColumn> = if let Some(m) = &cfg.mapping {
        if m.enabled {
            match mapper::load_asset_table(&m.source_path, &m.match_keys) {
                Ok(assets) => {
                    let index = mapper::build_asset_index(&assets);
                    for (i, rec) in records.iter().enumerate() {
                        let joined = mapper::join_record(rec, &index, m);
                        for (rename, val) in joined {
                            mapping_values.insert((i, rename), val);
                        }
                    }
                    // PRD §2.3：缺失锚点（非基础列）应记 Warning。
                    warnings.extend(mapper::missing_anchor_warnings(
                        mapper::BASE_COLUMNS,
                        &m.columns,
                    ));
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

    // 6. 渲染
    let spec = reporter::ReportSpec {
        base_columns: mapper::BASE_COLUMNS.iter().map(ToString::to_string).collect(),
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

fn parse_time(s: &str) -> Result<DateTime<Utc>, AppError> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .map(|ndt| DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
        .map_err(|_| AppError::TimeFormat { raw: s.into() })
}
