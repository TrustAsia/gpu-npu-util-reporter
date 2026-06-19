//! 配置模块：YAML 反序列化、带中文注释的默认配置生成、CLI 参数合并。
//!
//! 对应设计文档 §4.1。所有子结构都 derive `Serialize`/`Deserialize`，
//! 默认配置模板通过 [`default_config_yaml`] 产出。

use crate::devices::{ascend_910b_spec, nvidia_a10_spec, DeviceSpec};
use crate::error::AppError;
use crate::highlight::ThresholdTriggers;
use crate::mapper::MappingConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// CLI 可覆盖的运行参数（来自 clap）。
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub start: Option<String>,
    pub end: Option<String>,
    /// 记录配置文件来源路径（保留供诊断/日志，当前未在编排中读取）。
    #[allow(dead_code)]
    pub config_path: Option<String>,
    pub output: Option<String>,
}

/// 时间范围配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TimeRangeConfig {
    pub start: String,
    pub end: String,
}

/// 单个 Prometheus 数据源。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceConfig {
    /// 别名，写入"数据来源"列。
    pub name: String,
    pub url: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// 该源采集的设备类型 key（引用 devices 表）。
    pub device_types: Vec<String>,
}

fn default_timeout() -> u64 {
    30
}

/// 主机 IP 取值策略：优先标签，instance 兜底。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HostIpConfig {
    #[serde(default = "default_prefer_label")]
    pub prefer_label: String,
}

fn default_prefer_label() -> String {
    "ip".into()
}

/// 归属取值模式。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OwnershipConfig {
    #[serde(default = "default_mode")]
    pub mode: String, // "instant" | "last_in_range"
}

fn default_mode() -> String {
    "last_in_range".into()
}

/// 报表输出配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReportConfig {
    pub output_path: String,
    #[serde(default = "default_step")]
    pub query_step_secs: u64,
}

fn default_step() -> u64 {
    60
}

/// 应用顶层配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppConfig {
    pub time_range: TimeRangeConfig,
    pub sources: Vec<SourceConfig>,
    pub devices: HashMap<String, DeviceSpec>,
    pub host_ip: HostIpConfig,
    pub ownership: OwnershipConfig,
    #[serde(default)]
    pub mapping: Option<MappingConfig>,
    #[serde(default)]
    pub thresholds: ThresholdTriggers,
    pub report: ReportConfig,
}

/// 带中文注释的默认配置 YAML（开箱即用模板）。
///
/// 设备配方块直接由 serde 序列化 [`DeviceSpec`] 得到（缩进后嵌入），
/// 保证模板与反序列化器期望的 YAML 形态完全一致，`default_yaml_round_trips` 测试必通过。
///
/// 注：用普通字符串字面量 + `replace` 注入设备块，而不是 `format!`，因为模板里
/// 含大量 YAML 花括号（如 `{ after: "主机IP" }`、`["nvidia_a10", ...]`），
/// 这些会被 `format!` 误当成格式化参数。
pub fn default_config_yaml() -> String {
    // 注：用 r##"..."## 原始字符串，因为模板内含 `"#`（如 color: "#FF0000"），
    // r#" 会在第一个 "# 处提前结束。r## 允许内容里出现单个 "#。
    const TEMPLATE: &str = r##"# === GPU/NPU 利用率监控 默认配置 ===
# 时间范围（可被 --start/--end 覆盖），格式 YYYY-MM-DD HH:MM:SS
time_range:
  start: "2026-06-18 00:00:00"
  end:   "2026-06-19 00:00:00"

# Prometheus 数据源列表
sources:
  - name: "prod-cluster"
    url: "http://192.168.1.100:9090"
    timeout_secs: 30
    device_types: ["nvidia_a10", "ascend_910b"]

# 设备类型指标配方（含两套预设，可自定义新增）
# memory 用 untagged 表示：composite_ratio / direct_metric / composite_from_total
devices:
  nvidia_a10:
__NVIDIA__
  ascend_910b:
__ASCEND__

# 主机 IP 取值（标签优先，instance 兜底）
host_ip:
  prefer_label: "ip"

# 归属取值模式：instant 或 last_in_range
ownership:
  mode: "last_in_range"

# 资产映射（enabled: false 关闭）
mapping:
  enabled: false
  source_path: "./assets.csv"
  match_keys: ["host_ip", "card_id"]
  columns:
    - source_field: "机房位置"
      rename: "机房"
      position: { direction: after, anchor: "主机IP" }

# 阈值染色触发器（默认全为 null=未配置；启用时改为如下示例）
#   core_avg_above:
#     enabled: true
#     threshold: 80
#     color: "#FF0000"   # HEX，高于阈值染红（过载）
#   core_avg_below:
#     enabled: true
#     threshold: 10
#     color: "#FFA500"   # 低于阈值染橙（闲置）
thresholds:
  core_avg_above:    null
  core_avg_below:    null
  core_peak_above:   null
  core_peak_below:   null
  mem_avg_above:     null
  mem_avg_below:     null
  mem_peak_above:    null
  mem_peak_below:    null

# 报表输出
report:
  output_path: "./utilization-report.xlsx"
  query_step_secs: 60
"##;
    TEMPLATE
        .replace("__NVIDIA__", &indent_device(2, &nvidia_a10_spec()))
        .replace("__ASCEND__", &indent_device(2, &ascend_910b_spec()))
}

/// 把 DeviceSpec 序列化后按 `level` 层（每层 2 空格）缩进，嵌入到 `key:` 下方。
/// serde_yaml 顶层可能带一个 `---` 文档标记，需去掉。
fn indent_device(level: usize, spec: &DeviceSpec) -> String {
    let yaml = serde_yaml::to_string(spec).unwrap_or_default();
    let pad = " ".repeat(level * 2);
    yaml.lines()
        .filter(|l| !l.trim_start().starts_with("---"))
        .map(|l| format!("{pad}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 加载配置：若路径不存在则写出默认并返回 `Ok(None)` 让 main 提示退出。
pub fn load_or_init(path: &str) -> Result<Option<AppConfig>, AppError> {
    if !std::path::Path::new(path).exists() {
        std::fs::write(path, default_config_yaml()).map_err(|e| AppError::Config {
            path: path.into(),
            reason: format!("无法写入默认配置：{e}"),
        })?;
        return Ok(None);
    }
    let content = std::fs::read_to_string(path).map_err(|e| AppError::Config {
        path: path.into(),
        reason: format!("读取失败：{e}"),
    })?;
    let cfg: AppConfig = serde_yaml::from_str(&content).map_err(|e| AppError::Config {
        path: path.into(),
        reason: format!("{e}"),
    })?;
    Ok(Some(cfg))
}

/// 用 CLI 覆盖配置字段（start/end/output）。
/// 要求：start 与 end 必须同时给或同时不给。
pub fn apply_overrides(mut cfg: AppConfig, ov: &CliOverrides) -> Result<AppConfig, AppError> {
    match (&ov.start, &ov.end) {
        (Some(s), Some(e)) => {
            validate_time(s)?;
            validate_time(e)?;
            cfg.time_range.start = s.clone();
            cfg.time_range.end = e.clone();
        }
        (None, None) => {}
        _ => {
            return Err(AppError::Config {
                path: "<cli>".into(),
                reason: "--start 与 --end 必须同时提供".into(),
            });
        }
    }
    if let Some(o) = &ov.output {
        cfg.report.output_path = o.clone();
    }
    Ok(cfg)
}

/// 校验时间字符串格式。
fn validate_time(s: &str) -> Result<(), AppError> {
    use chrono::NaiveDateTime;
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").map_err(|_| AppError::TimeFormat {
        raw: s.into(),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_yaml_round_trips() {
        let yaml = default_config_yaml();
        let cfg: AppConfig = serde_yaml::from_str(&yaml).expect("默认 YAML 必须可解析");
        assert_eq!(cfg.devices.get("nvidia_a10").unwrap().card_id_label, "gpu");
        assert_eq!(cfg.devices.get("ascend_910b").unwrap().card_id_label, "id");
        assert!(cfg.thresholds.core_avg_above.is_none()); // 默认模板里 thresholds 全为 null
    }

    #[test]
    fn apply_overrides_requires_both_start_and_end() {
        let cfg = serde_yaml::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        let r = apply_overrides(
            cfg,
            &CliOverrides {
                start: Some("2026-01-01 00:00:00".into()),
                end: None,
                config_path: None,
                output: None,
            },
        );
        assert!(r.is_err());
    }

    #[test]
    fn apply_overrides_accepts_valid_times() {
        let cfg = serde_yaml::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        let out = apply_overrides(
            cfg,
            &CliOverrides {
                start: Some("2026-01-01 00:00:00".into()),
                end: Some("2026-01-02 00:00:00".into()),
                config_path: None,
                output: Some("./out.xlsx".into()),
            },
        )
        .unwrap();
        assert_eq!(out.time_range.start, "2026-01-01 00:00:00");
        assert_eq!(out.report.output_path, "./out.xlsx");
    }

    #[test]
    fn validate_time_rejects_bad_format() {
        assert!(validate_time("2026/01/01 00:00:00").is_err());
        assert!(validate_time("2026-01-01 00:00:00").is_ok());
    }
}
