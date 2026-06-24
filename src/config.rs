//! 配置模块：YAML 反序列化、带中文注释的默认配置生成、CLI 参数合并。
//!
//! 对应设计文档 §4.1。所有子结构都 derive `Serialize`/`Deserialize`，
//! 默认配置模板通过 [`default_config_yaml`] 产出。

use crate::devices::{ascend_910b_spec, nvidia_a10_spec, DeviceSpec};
use crate::error::AppError;
use crate::highlight::ThresholdTriggers;
use crate::mapper::MappingConfig;
use chrono::NaiveDateTime;
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TimeRangeConfig {
    pub start: String,
    pub end: String,
}

/// 单个 Prometheus 数据源。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    /// 别名，写入"数据来源"列。
    pub name: String,
    pub url: String,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// 该源采集的设备类型 key（引用 devices 表）。
    pub device_types: Vec<String>,
}

const fn default_timeout() -> u64 {
    30
}

/// 主机 IP 取值策略：优先指定标签，instance 兜底。
///
/// 注：主机 IP 标签名现已纳入各设备配方的 `labels.host_ip` 字段，
/// 不再需要独立的 `host_ip` 配置块。此结构体仅用于从旧配置文件
/// 向后兼容反序列化（忽略即可）。新增设备类型时直接在 `labels` 里配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HostIpConfig {
    #[serde(default)]
    pub prefer_label: String,
}

/// 归属取值模式。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OwnershipConfig {
    #[serde(default = "default_mode")]
    pub mode: String, // "instant" | "last_in_range"
}

fn default_mode() -> String {
    "last_in_range".into()
}

/// 日志配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LogConfig {
    /// 控制台日志级别：trace/debug/info/warn/error
    #[serde(default = "default_console_level")]
    pub console_level: String,
    /// 是否启用文件日志。
    #[serde(default)]
    pub file_enabled: bool,
    /// 文件日志级别。
    #[serde(default = "default_file_level")]
    pub file_level: String,
    /// 日志文件路径（支持模板变量 {{start}}, {{end}}, {{now}} 等）。
    #[serde(default = "default_log_path")]
    pub file_path: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            console_level: default_console_level(),
            file_enabled: false,
            file_level: default_file_level(),
            file_path: default_log_path(),
        }
    }
}

fn default_console_level() -> String {
    "info".into()
}

fn default_file_level() -> String {
    "debug".into()
}

fn default_log_path() -> String {
    "./logs/{{now}}.log".into()
}

/// 主机指标采集配置（通用指标，不绑定设备类型）。
///
/// 启用后对每个唯一的主机 IP 查询 CPU/内存/句柄数利用率，
/// 结果填入该主机下所有计算卡行。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostMetricsConfig {
    /// 是否启用主机指标采集。
    #[serde(default)]
    pub enabled: bool,
    /// 指定从哪个数据源查询主机指标（按 name 匹配）；不指定时使用第一个数据源。
    #[serde(default)]
    pub source: Option<String>,
    /// CPU 利用率 Prometheus 指标名。
    pub cpu_metric: String,
    /// 内存利用率 Prometheus 指标名。
    pub mem_metric: String,
    /// 句柄数 Prometheus 指标名（可选）。
    #[serde(default)]
    pub handle_metric: Option<String>,
    /// Prometheus 标签名，用于匹配主机 IP（默认 "instance"）。
    #[serde(default = "default_host_label")]
    pub host_label: String,
}

fn default_host_label() -> String {
    "instance".into()
}

/// 报表输出配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReportConfig {
    /// 报表输出路径（支持模板变量 {{start}}, {{end}}, {{now}} 等）。
    pub output_path: String,
    #[serde(default = "default_step")]
    pub query_step_secs: u64,
}

const fn default_step() -> u64 {
    60
}

/// 应用顶层配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub time_range: TimeRangeConfig,
    /// 显示时区（IANA 时区名，如 `Asia/Shanghai`、`UTC`）。
    /// 影响报表时间列、模板变量渲染、日志输出；不影响 Prometheus API 交互（始终 UTC）。
    #[serde(default = "default_timezone")]
    pub timezone: String,
    pub sources: Vec<SourceConfig>,
    pub devices: HashMap<String, DeviceSpec>,
    /// 向后兼容：旧配置文件中的 `host_ip` 块。主机 IP 标签名现已纳入
    /// 各设备配方的 `labels.host_ip` 字段，此字段仅用于旧配置反序列化不报错。
    #[serde(default)]
    pub host_ip: HostIpConfig,
    pub ownership: OwnershipConfig,
    #[serde(default)]
    pub mapping: Option<MappingConfig>,
    #[serde(default)]
    pub thresholds: ThresholdTriggers,
    #[serde(default)]
    pub log: LogConfig,
    pub report: ReportConfig,
    /// 主机指标采集配置（可选）。
    #[serde(default)]
    pub host_metrics: Option<HostMetricsConfig>,
}

fn default_timezone() -> String {
    "Asia/Shanghai".into()
}

/// 带中文注释的默认配置 YAML（开箱即用模板）。
///
/// 设备配方块直接由 serde 序列化 [`DeviceSpec`] 得到（缩进后嵌入），
/// 保证模板与反序列化器期望的 YAML 形态完全一致，`default_yaml_round_trips` 测试必通过。
///
/// 注：用普通字符串字面量 + `replace` 注入设备块，而不是 `format!`，因为模板里
/// 含大量 YAML 花括号（如 `{ after: "主机IP" }`、`["nvidia_a10", ...]`），
/// 这些会被 `format!` 误当成格式化参数。
#[must_use]
pub fn default_config_yaml() -> String {
    // 注：用 r##"..."## 原始字符串，因为模板内含 `"#`（如 color: "#FF0000"），
    // r#" 会在第一个 "# 处提前结束。r## 允许内容里出现单个 "#。
    const TEMPLATE: &str = r##"# =============================================================================
# GPU/NPU 利用率监控报表 — 配置文件
# =============================================================================
#
# 本文件定义数据源、设备指标配方、资产映射、阈值染色等全部运行参数。
# 修改后直接运行 gpu-npu-util-reporter 即可生效，
# 也可通过 --start/--end/--output 命令行参数临时覆盖。
#
# =============================================================================
# 一、时间范围 — 定义 Prometheus 查询窗口
# =============================================================================
# 支持两种格式：
#   绝对时间 — "YYYY-MM-DD HH:MM:SS"（如 "2026-06-24 00:00:00"）
#   相对时间 — 以 now / start / end 为锚点，加减时间偏移量。
#
#   相对时间语法：<锚点>[+|-]<数值><单位>
#     锚点：now=当前时刻  start=起始时间  end=结束时间
#     单位：s(秒) m(分) h(时) d(天) w(周) M(月) y(年)
#
#   示例：
#     now-7d   → 7 天前
#     now      → 当前时刻
#     end-1h   → 结束时间前 1 小时（要求 end 已能独立解析）
#     start+3h → 起始时间后 3 小时
#
#   提示：若 start 引用了 end（如 "end-1d"），系统会先解析 end 再重试 start。
#         CLI 参数 --start / --end 同时提供时覆盖此处的值。
#
time_range:
  start: "now-1d"
  end:   "now"

# =============================================================================
# 二、显示时区 — 决定报表中时间的展示，不影响 Prometheus 查询（查询始终 UTC）
# =============================================================================
# 取值为 IANA 时区名，常用值：Asia/Shanghai（UTC+8）、UTC、America/New_York。
# 影响范围：报表所有时间列、模板变量 {{start}}/{{end}}/{{now}} 的格式化。
#
timezone: "Asia/Shanghai"

# =============================================================================
# 三、Prometheus 数据源 — 可配置多个，按顺序独立采集
# =============================================================================
# 每个数据源对应一个 Prometheus 实例：
#   name          — 别名，会写入报表「数据来源」列，用于区分多集群
#   url           — Prometheus HTTP API 地址，必须以 http:// 或 https:// 开头
#   timeout_secs  — 单次 HTTP 请求超时（秒），默认 30
#   device_types  — 该源覆盖的设备类型列表，引用下方 devices 块中的 key
#                   每个 key 会发起独立的 PromQL 查询，互不干扰
#
# 示例：下方配置表示从 prod-cluster 采集 nvidia_a10 和 ascend_910b 两种设备。
#
sources:
  - name: "prod-cluster"
    url: "http://192.168.1.100:9090"
    timeout_secs: 30
    device_types: ["nvidia_a10", "ascend_910b"]

# =============================================================================
# 四、设备类型指标配方 — 定义每种设备的 Prometheus 指标和标签映射
# =============================================================================
#
# 系统据此完成三件事：
#   1. 构造 PromQL 查询（指标名 + 标签过滤）
#   2. 从返回时序提取归属信息（namespace/pod/container）和设备属性
#   3. 计算核心利用率、显存占用率、温度、功率等衍生指标
#
# ---- 字段说明（每个设备类型均需提供）----------------------------------------
#
# display_name     — 报表「设备类型」列显示名，如 "NVIDIA A10"、"Ascend 910B"
#
# core_util_metric — 核心利用率 Prometheus 指标名。要求该指标值本身已在 0–100
#                    范围内（百分比），系统直接取平均值/峰值，不做额外转换。
#
# memory           — 显存占用率计算策略。系统根据 YAML 键名自动识别三种变体
#                    （#[serde(untagged)]），无需显式声明 type：
#
#   a) composite_ratio — 组合公式：used / (used + free) × 100
#      适用场景：显存数据分为"已用"和"空闲"两个独立 Counter/Gauge 指标。
#      示例（NVIDIA DCGM）：
#        composite_ratio:
#          used: "DCGM_FI_DEV_FB_USED"
#          free: "DCGM_FI_DEV_FB_FREE"
#
#   b) direct_metric — 直接读取一个现成的利用率指标（值已在 0–100 范围）
#      适用场景：exporter 已直接暴露百分比利用率。
#      可选 fallback：当主指标查询结果为空时，自动切换到备选策略。
#      示例（NPU Exporter，带 fallback）：
#        direct_metric:
#          metric: "npu_chip_info_hbm_utilization"
#          fallback:                          # 可选；主指标无数据时自动启用
#            composite_from_total:
#              used: "npu_chip_info_hbm_used_memory"
#              total: "npu_chip_info_hbm_total_memory"
#
#   c) composite_from_total — 组合公式：used / total × 100
#      适用场景：有"已用"和"总量"两个独立指标，但没有空闲量指标。
#      示例：
#        composite_from_total:
#          used: "npu_chip_info_hbm_used_memory"
#          total: "npu_chip_info_hbm_total_memory"
#
# card_id_label    — 卡编号所在的 Prometheus 标签名。
#                    如 NVIDIA DCGM 通常用 gpu 标签（值 0,1,2…），
#                    NPU Exporter 通常用 id 标签。
#                    系统通过此标签区分同一主机下的不同物理卡。
#
# labels           — 归属标签映射。将统一的逻辑字段名映射到各 exporter
#                    实际使用的 Prometheus 标签名：
#     host_ip   — 主机 IP 标签。优先取此标签值；取不到时从 instance 标签
#                 去端口解析（如 "192.168.1.100:9100" → "192.168.1.100"）
#     node_name — 节点名称标签（如 Kubernetes node 名）
#     container — 容器名标签
#     pod       — Pod 名标签
#     namespace — Namespace 标签
#
# temp_metric      — 设备温度 Prometheus 指标名（可选）。
#                    配置后报表自动新增「设备温度平均值/峰值/…」等 6 列。
#                    例如 NVIDIA 用 "DCGM_FI_DEV_GPU_TEMP"，
#                    NPU 用 "npu_chip_info_temperature"。
#
# power_metric     — 设备功率 Prometheus 指标名（可选）。
#                    配置后报表自动新增「设备功率平均值/峰值/…」等 6 列。
#                    例如 NVIDIA 用 "DCGM_FI_DEV_POWER_USAGE"，
#                    NPU 用 "npu_chip_info_power"。
#
# ---- 新增自定义设备类型 ------------------------------------------------------
# 在 devices 块下新增一个 key（如 my_device），按上方字段填写即可，无需改代码。
#
devices:
  nvidia_a10:
__NVIDIA__
  ascend_910b:
__ASCEND__

# =============================================================================
# 五、归属取值模式 — 决定每条卡记录的 namespace/pod/container 如何确定
# =============================================================================
#
# 同一张卡上可能在不同时间运行不同 Pod，因此需要策略决定"最终归属谁"：
#
#   instant       — 取单个瞬时点（end 时刻或最近时刻）的标签值。
#                   速度快，但可能遗漏时间窗口内的容器切换。
#
#   last_in_range — 在查询时间范围内取最后一条数据的标签值（默认，推荐）。
#                   反映的是时间范围结束时的实际归属状态。
#
ownership:
  mode: "last_in_range"

# =============================================================================
# 六、资产映射 — 将外部资产表（CSV/Excel）的信息关联到报表行（可选）
# =============================================================================
#
# 典型场景：将主机 IP 映射到机房位置、负责人、采购日期等信息，
# 让报表不仅展示利用率数据，还包含资产的业务属性。
#
# enabled: false   — 设为 true 启用，false 则整个映射模块跳过不执行。
#
# ---- 匹配机制 ---------------------------------------------------------------
# match_keys — 资产表中的列名（如 "IP地址"、"host_ip" 等）
# record_key — CardRecord 中对应的字段名（可选）。
#              支持的字段：source_name, host_ip, node_name, card_id,
#                         device_type, namespace, pod, container
#              不指定时默认与 match_keys 相同。
#
#   示例 1：资产表用 "host_ip" 列 → match_keys: "host_ip" 即可
#   示例 2：资产表用 "IP地址" 列 → match_keys: "IP地址", record_key: "host_ip"
#
# ---- 列映射 -----------------------------------------------------------------
# columns 定义从资产表提取哪些列，以及它们插入报表的位置和显示名称：
#   source_field — 资产表中的源列名
#   rename       — 报表列显示名（可与 source_field 不同，如"机房位置"→"机房"）
#   position     — 插入位置
#     direction  — after（在锚点列之后）或 before（在锚点列之前）
#     anchor     — 锚点列名（报表现有列名，如 "主机IP"、"设备类型"等）
#
# 支持多来源：可配置多个 MappingSource，每个引用独立的资产表文件。
# source_sheet 可选：指定 Excel 工作表名；不指定时默认读取第一个工作表。
#
mapping:
  enabled: false
  sources:
    - source_path: "./assets.csv"
      match_keys: "host_ip"
      columns:
        - source_field: "机房位置"
          rename: "机房"
          position: { direction: after, anchor: "主机IP" }

# =============================================================================
# 七、阈值染色触发器 — 在 Excel 中用背景色标记满足条件的单元格
# =============================================================================
#
# 每个触发器 3 个字段：
#   enabled   — true 启用；false 或 null（空）表示关闭
#   threshold — 阈值。利用率类指标为百分比（0–100），温度/功率/句柄数为绝对值
#   color     — HEX 颜色 #RRGGBB（大写），如 "#FF0000"=红色 "#FFA500"=橙色
#
# 触发器按指标分为 8 组，共 32 个：
#
#  组 1 — 核心利用率（百分比）
#    core_avg_above  / core_avg_below   — 平均值 高于/低于 阈值
#    core_peak_above / core_peak_below  — 峰值   高于/低于 阈值
#
#  组 2 — 显存占用率（百分比）
#    mem_avg_above  / mem_avg_below     — 平均值
#    mem_peak_above / mem_peak_below    — 峰值
#
#  组 3 — 设备温度（摄氏度，绝对值）
#    temp_avg_above  / temp_avg_below   — 平均值
#    temp_peak_above / temp_peak_below  — 峰值
#
#  组 4 — 设备功率（瓦特，绝对值）
#    power_avg_above  / power_avg_below  — 平均值
#    power_peak_above / power_peak_below — 峰值
#
#  组 5 — 主机 CPU 利用率（百分比）
#    host_cpu_avg_above  / host_cpu_avg_below   — 平均值
#    host_cpu_peak_above / host_cpu_peak_below  — 峰值
#
#  组 6 — 主机内存利用率（百分比）
#    host_mem_avg_above  / host_mem_avg_below   — 平均值
#    host_mem_peak_above / host_mem_peak_below  — 峰值
#
#  组 7 — 主机句柄数（绝对值）
#    host_handle_avg_above  / host_handle_avg_below   — 平均值
#    host_handle_peak_above / host_handle_peak_below  — 峰值
#
# ---- 启用示例（取消注释并填入具体值）----------------------------------------
# thresholds:
#   core_avg_above:
#     enabled: true
#     threshold: 80
#     color: "#FF0000"     # 平均利用率 > 80% → 红色告警（过载）
#   core_avg_below:
#     enabled: true
#     threshold: 10
#     color: "#FFA500"     # 平均利用率 < 10% → 橙色告警（闲置）
#
thresholds:
  # -- 核心利用率 --
  core_avg_above:      null
  core_avg_below:      null
  core_peak_above:     null
  core_peak_below:     null
  # -- 显存占用率 --
  mem_avg_above:       null
  mem_avg_below:       null
  mem_peak_above:      null
  mem_peak_below:      null
  # -- 设备温度 --
  temp_avg_above:      null
  temp_avg_below:      null
  temp_peak_above:     null
  temp_peak_below:     null
  # -- 设备功率 --
  power_avg_above:     null
  power_avg_below:     null
  power_peak_above:    null
  power_peak_below:    null
  # -- 主机 CPU --
  host_cpu_avg_above:  null
  host_cpu_avg_below:  null
  host_cpu_peak_above: null
  host_cpu_peak_below: null
  # -- 主机内存 --
  host_mem_avg_above:    null
  host_mem_avg_below:    null
  host_mem_peak_above:   null
  host_mem_peak_below:   null
  # -- 主机句柄数 --
  host_handle_avg_above:  null
  host_handle_avg_below:  null
  host_handle_peak_above: null
  host_handle_peak_below: null

# =============================================================================
# 八、主机指标采集 — 以主机为粒度的 CPU/内存/句柄数（可选，通用指标）
# =============================================================================
#
# 与设备指标不同：主机指标不绑定到某一种设备类型，而是按主机 IP 粒度采集，
# 结果填入该 IP 下所有计算卡行（同一主机的多张卡看到的主机指标值相同）。
#
# ---- 查询机制 ---------------------------------------------------------------
# 系统从已采集的卡记录中提取所有唯一主机 IP，对每个 IP 构造 PromQL 查询：
#   <指标名>{<host_label>=~"^<转义后IP>.*"}
# 通过 regex 锚定主机 IP 前缀（如 ^192\.168\.1\.100.*），实现主机级聚合。
#
# ---- 字段说明 ---------------------------------------------------------------
# enabled       — 是否启用。默认 false，设为 true 开启。
# source        — 从哪个数据源查询主机指标（按 sources 中的 name 匹配）。
#                 不指定时自动使用第一个数据源。
# cpu_metric    — CPU 利用率 Prometheus 指标名（值应在 0–100 范围）。
# mem_metric    — 内存利用率 Prometheus 指标名（值应在 0–100 范围）。
# handle_metric — 句柄数 Prometheus 指标名（可选，不配置则跳过句柄数采集）。
# host_label    — Prometheus 标签名，用于匹配主机 IP。
#                 默认 "instance"（Prometheus 自动附加的抓取目标标签）。
#                 如果 exporter 用不同标签标识主机（如 "host"、"node"），
#                 修改此字段即可，无需改代码。
#
# host_metrics:
#   enabled: false
#   source: "prod-cluster"
#   cpu_metric: "node_cpu_utilization"
#   mem_metric: "node_memory_utilization"
#   handle_metric: "node_filefd_allocated"
#   host_label: "instance"

# =============================================================================
# 九、日志配置
# =============================================================================
# console_level — 控制台输出级别：trace / debug / info / warn / error
#                 建议日常 info，排查问题时可临时改为 debug。
# file_enabled  — 是否启用文件日志（日志文件路径支持模板变量，见下文）。
# file_level    — 文件日志级别，通常比控制台更详细（默认 debug）。
# file_path     — 日志文件路径，支持模板变量。
#
log:
  console_level: "info"
  file_enabled: false
  file_level: "debug"
  file_path: "./logs/{{now}}.log"

# =============================================================================
# 十、报表输出
# =============================================================================
# output_path      — 输出 .xlsx 文件路径，支持模板变量。
# query_step_secs  — Prometheus query_range API 的查询步长（秒）。
#                    决定返回时序的数据点密度：步长越小 → 数据点越多 → 聚合越精确，
#                    但 Prometheus 查询耗时和内存占用也越大。建议 60–300 秒。
#
# ---- 模板变量参考（可用于 output_path 与 log.file_path）--------------------
#
#  以 start 为例（end / now 同理）：
#    {{start}}         → 2026-06-24_00-00-00  （完整时间，: 替换为 -）
#    {{start_date}}    → 2026-06-24           （仅日期）
#    {{start_time}}    → 00-00-00             （仅时间）
#    {{start_year}}    → 2026                 （年）
#    {{start_month}}   → 06                   （月）
#    {{start_day}}     → 24                   （日）
#    {{start_hour}}    → 00                   （时）
#    {{start_minute}}  → 00                   （分）
#    {{start_second}}  → 00                   （秒）
#
#  注意：所有模板变量均按上方 timezone 配置的时区格式化。
#        未识别的变量（如 {{unknown}}）会原样保留在路径中。
#
report:
  output_path: "./utilization-report.xlsx"
  query_step_secs: 60
"##;
    TEMPLATE
        .replace("__NVIDIA__", &indent_device(2, &nvidia_a10_spec()))
        .replace("__ASCEND__", &indent_device(2, &ascend_910b_spec()))
}

/// 带 `DeviceSpec` 序列化后按 `level` 层（每层 2 空格）缩进，嵌入到 `key:` 下方。
/// `serde_yaml_ng` 顶层可能带一个 `---` 文档标记，需去掉。
fn indent_device(level: usize, spec: &DeviceSpec) -> String {
    let yaml = serde_yaml_ng::to_string(spec).unwrap_or_default();
    let pad = " ".repeat(level * 2);
    yaml.lines()
        .filter(|l| !l.trim_start().starts_with("---"))
        .map(|l| format!("{pad}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 加载配置：若路径不存在则写出默认并返回 `Ok(None)` 让 main 提示退出。
///
/// # Errors
///
/// 返回 [`AppError::Config`] 当文件读取失败或 YAML 解析失败。
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
    let cfg: AppConfig = serde_yaml_ng::from_str(&content).map_err(|e| AppError::Config {
        path: path.into(),
        reason: format!("{e}"),
    })?;
    validate_config(&cfg, path)?;
    Ok(Some(cfg))
}

/// 校验配置合法性。
#[allow(clippy::too_many_lines)]
fn validate_config(cfg: &AppConfig, path: &str) -> Result<(), AppError> {
    // 校验时区名合法性
    if cfg.timezone.parse::<chrono_tz::Tz>().is_err() {
        return Err(AppError::Config {
            path: path.into(),
            reason: format!(
                "timezone「{}」不是合法的 IANA 时区名（如 Asia/Shanghai、UTC）",
                cfg.timezone
            ),
        });
    }
    if cfg.report.query_step_secs == 0 {
        return Err(AppError::Config {
            path: path.into(),
            reason: "report.query_step_secs 必须 > 0".into(),
        });
    }
    if cfg.report.query_step_secs > (i64::MAX / 1_000) as u64 {
        return Err(AppError::Config {
            path: path.into(),
            reason: format!("report.query_step_secs 过大（最大 {}）", i64::MAX / 1_000),
        });
    }
    if cfg.sources.iter().any(|s| s.timeout_secs == 0) {
        return Err(AppError::Config {
            path: path.into(),
            reason: "sources[].timeout_secs 必须 > 0".into(),
        });
    }
    if cfg.sources.is_empty() {
        return Err(AppError::Config {
            path: path.into(),
            reason: "sources 不能为空".into(),
        });
    }
    for src in &cfg.sources {
        if !src.url.starts_with("http://") && !src.url.starts_with("https://") {
            return Err(AppError::Config {
                path: path.into(),
                reason: format!(
                    "数据源「{}」的 url 必须以 http:// 或 https:// 开头（当前：{}）",
                    src.name,
                    crate::error::AppError::redact_url(&src.url)
                ),
            });
        }
    }
    // 校验时间范围逻辑：start 必须早于 end，否则 Prometheus 返回空数据。
    // 如果两个值都是绝对时间，立即校验；含相对时间的表达式在运行时解析后校验。
    let start = NaiveDateTime::parse_from_str(&cfg.time_range.start, "%Y-%m-%d %H:%M:%S");
    let end = NaiveDateTime::parse_from_str(&cfg.time_range.end, "%Y-%m-%d %H:%M:%S");
    if let (Ok(s), Ok(e)) = (start, end) {
        if s >= e {
            return Err(AppError::Config {
                path: path.into(),
                reason: "time_range.start 必须早于 time_range.end".into(),
            });
        }
    }
    // 校验时间字段：必须是绝对时间或合法的相对时间表达式
    validate_time_or_expr(&cfg.time_range.start).map_err(|e| AppError::Config {
        path: path.into(),
        reason: format!("time_range.start：{e}"),
    })?;
    validate_time_or_expr(&cfg.time_range.end).map_err(|e| AppError::Config {
        path: path.into(),
        reason: format!("time_range.end：{e}"),
    })?;
    // 校验设备配方中指标名/标签名的合法性，防止 PromQL 注入。
    // Prometheus 指标名: [a-zA-Z_:][a-zA-Z0-9_:]*
    // 标签名: [a-zA-Z_][a-zA-Z0-9_]*
    for (key, spec) in &cfg.devices {
        if !is_valid_metric_name(&spec.core_util_metric) {
            return Err(AppError::Config {
                path: path.into(),
                reason: format!(
                    "devices.{}.core_util_metric「{}」不是合法的 Prometheus 指标名（仅允许字母/数字/下划线/冒号）",
                    key, spec.core_util_metric
                ),
            });
        }
        if !is_valid_label_name(&spec.card_id_label) {
            return Err(AppError::Config {
                path: path.into(),
                reason: format!(
                    "devices.{}.card_id_label「{}」不是合法的 Prometheus 标签名（仅允许字母/数字/下划线）",
                    key, spec.card_id_label
                ),
            });
        }
        if !is_valid_label_name(&spec.labels.host_ip) {
            return Err(AppError::Config {
                path: path.into(),
                reason: format!(
                    "devices.{}.labels.host_ip「{}」不是合法的 Prometheus 标签名",
                    key, spec.labels.host_ip
                ),
            });
        }
        if !is_valid_label_name(&spec.labels.node_name) {
            return Err(AppError::Config {
                path: path.into(),
                reason: format!(
                    "devices.{}.labels.node_name「{}」不是合法的 Prometheus 标签名",
                    key, spec.labels.node_name
                ),
            });
        }
        if !is_valid_label_name(&spec.labels.container) {
            return Err(AppError::Config {
                path: path.into(),
                reason: format!(
                    "devices.{}.labels.container「{}」不是合法的 Prometheus 标签名",
                    key, spec.labels.container
                ),
            });
        }
        if !is_valid_label_name(&spec.labels.pod) {
            return Err(AppError::Config {
                path: path.into(),
                reason: format!(
                    "devices.{}.labels.pod「{}」不是合法的 Prometheus 标签名",
                    key, spec.labels.pod
                ),
            });
        }
        if !is_valid_label_name(&spec.labels.namespace) {
            return Err(AppError::Config {
                path: path.into(),
                reason: format!(
                    "devices.{}.labels.namespace「{}」不是合法的 Prometheus 标签名",
                    key, spec.labels.namespace
                ),
            });
        }
        // 校验显存策略中的指标名
        validate_memory_metrics(&spec.memory, key, path)?;
        // 校验温度/功率指标名（可选）
        if let Some(tm) = &spec.temp_metric {
            if !is_valid_metric_name(tm) {
                return Err(AppError::Config {
                    path: path.into(),
                    reason: format!(
                        "devices.{key}.temp_metric「{tm}」不是合法的 Prometheus 指标名"
                    ),
                });
            }
        }
        if let Some(pm) = &spec.power_metric {
            if !is_valid_metric_name(pm) {
                return Err(AppError::Config {
                    path: path.into(),
                    reason: format!(
                        "devices.{key}.power_metric「{pm}」不是合法的 Prometheus 指标名"
                    ),
                });
            }
        }
    }
    // 校验 sources[].device_types 引用的设备类型在 devices 中存在
    for src in &cfg.sources {
        for dt in &src.device_types {
            if !cfg.devices.contains_key(dt) {
                return Err(AppError::Config {
                    path: path.into(),
                    reason: format!(
                        "数据源「{}」的 device_types 引用了未定义的设备类型「{}」",
                        src.name, dt
                    ),
                });
            }
        }
    }
    // 校验主机指标配置
    if let Some(hm) = &cfg.host_metrics {
        if hm.enabled {
            if !is_valid_metric_name(&hm.cpu_metric) {
                return Err(AppError::Config {
                    path: path.into(),
                    reason: format!(
                        "host_metrics.cpu_metric「{}」不是合法的 Prometheus 指标名",
                        hm.cpu_metric
                    ),
                });
            }
            if !is_valid_metric_name(&hm.mem_metric) {
                return Err(AppError::Config {
                    path: path.into(),
                    reason: format!(
                        "host_metrics.mem_metric「{}」不是合法的 Prometheus 指标名",
                        hm.mem_metric
                    ),
                });
            }
            if let Some(handle) = &hm.handle_metric {
                if !is_valid_metric_name(handle) {
                    return Err(AppError::Config {
                        path: path.into(),
                        reason: format!(
                            "host_metrics.handle_metric「{handle}」不是合法的 Prometheus 指标名"
                        ),
                    });
                }
            }
            if !is_valid_label_name(&hm.host_label) {
                return Err(AppError::Config {
                    path: path.into(),
                    reason: format!(
                        "host_metrics.host_label「{}」不是合法的 Prometheus 标签名",
                        hm.host_label
                    ),
                });
            }
            // 校验 source 引用的数据源存在
            if let Some(ref src_name) = hm.source {
                if !cfg.sources.iter().any(|s| &s.name == src_name) {
                    return Err(AppError::Config {
                        path: path.into(),
                        reason: format!("host_metrics.source「{src_name}」引用了未定义的数据源"),
                    });
                }
            }
        }
    }

    // 校验映射配置中的 record_key / match_keys 字段名合法性
    // 仅在 mapping.enabled=true 时校验；disabled 的映射不应阻断配置加载
    if let Some(mapping) = &cfg.mapping {
        if mapping.enabled {
            for src in &mapping.sources {
                if src.match_keys.is_empty() {
                    return Err(AppError::Config {
                        path: path.into(),
                        reason: format!(
                            "映射来源「{}」的 match_keys 不能为空字符串",
                            src.source_path
                        ),
                    });
                }
                let card_record_field = src.record_key.as_deref().unwrap_or(&src.match_keys);
                if !crate::mapper::KNOWN_CARD_RECORD_FIELDS.contains(&card_record_field) {
                    return Err(AppError::Config {
                        path: path.into(),
                        reason: format!(
                        "映射来源「{}」的 CardRecord 字段名「{}」不在已知字段列表中（支持：{}）",
                        src.source_path,
                        card_record_field,
                        crate::mapper::KNOWN_CARD_RECORD_FIELDS.join(", ")
                    ),
                    });
                }
            }
            // 检测映射列 rename 重复
            let dup_warnings = mapping.duplicate_rename_warnings();
            if let Some(first) = dup_warnings.first() {
                return Err(AppError::Config {
                    path: path.into(),
                    reason: first.clone(),
                });
            }
        }
    }
    Ok(())
}

/// 校验显存策略中所有指标名的合法性（递归，因 fallback 嵌套）。
/// 同时校验 fallback 嵌套深度不超过 [`MAX_FALLBACK_DEPTH`]，防止栈溢出。
const MAX_FALLBACK_DEPTH: usize = 10;

fn validate_memory_metrics(
    strategy: &crate::devices::MemoryStrategy,
    device_key: &str,
    path: &str,
) -> Result<(), AppError> {
    validate_memory_metrics_inner(strategy, device_key, path, 0)
}

fn validate_memory_metrics_inner(
    strategy: &crate::devices::MemoryStrategy,
    device_key: &str,
    path: &str,
    depth: usize,
) -> Result<(), AppError> {
    if depth > MAX_FALLBACK_DEPTH {
        return Err(AppError::Config {
            path: path.into(),
            reason: format!(
                "devices.{device_key}.memory fallback 嵌套深度超过 {MAX_FALLBACK_DEPTH} 层，请检查配置"
            ),
        });
    }
    match strategy {
        crate::devices::MemoryStrategy::CompositeRatio(b) => {
            for name in [&b.composite_ratio.used, &b.composite_ratio.free] {
                if !is_valid_metric_name(name) {
                    return Err(AppError::Config {
                        path: path.into(),
                        reason: format!(
                            "devices.{device_key}.memory 指标名「{name}」不合法（仅允许字母/数字/下划线/冒号）"
                        ),
                    });
                }
            }
        }
        crate::devices::MemoryStrategy::DirectMetric(b) => {
            if !is_valid_metric_name(&b.direct_metric.metric) {
                return Err(AppError::Config {
                    path: path.into(),
                    reason: format!(
                        "devices.{}.memory 指标名「{}」不合法",
                        device_key, b.direct_metric.metric
                    ),
                });
            }
            if let Some(fb) = &b.direct_metric.fallback {
                validate_memory_metrics_inner(fb, device_key, path, depth + 1)?;
            }
        }
        crate::devices::MemoryStrategy::CompositeFromTotal(b) => {
            for name in [&b.composite_from_total.used, &b.composite_from_total.total] {
                if !is_valid_metric_name(name) {
                    return Err(AppError::Config {
                        path: path.into(),
                        reason: format!("devices.{device_key}.memory 指标名「{name}」不合法"),
                    });
                }
            }
        }
    }
    Ok(())
}

/// Prometheus 指标名合法性：`[a-zA-Z_:][a-zA-Z0-9_:]*`
fn is_valid_metric_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() && first != '_' && first != ':' {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

/// Prometheus 标签名合法性：`[a-zA-Z_][a-zA-Z0-9_]*`
fn is_valid_label_name(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphabetic() && first != '_' {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// 用 CLI 覆盖配置字段（start/end/output）。
/// 要求：start 与 end 必须同时给或同时不给。
///
/// # Errors
///
/// 返回 [`AppError::Config`] 当 start/end 只给了一个，或时间格式无效。
pub fn apply_overrides(mut cfg: AppConfig, ov: &CliOverrides) -> Result<AppConfig, AppError> {
    match (&ov.start, &ov.end) {
        (Some(s), Some(e)) => {
            validate_time_or_expr(s)?;
            validate_time_or_expr(e)?;
            // CLI 覆盖也需校验 start < end（配置文件的校验在 load_or_init 里，
            // 但 CLI 覆盖发生在之后，如果不重新校验会绕过约束）。
            // 注意：相对时间表达式在 apply_overrides 阶段不解析，
            // start < end 校验在 main 中解析绝对时间后进行。
            cfg.time_range.start.clone_from(s);
            cfg.time_range.end.clone_from(e);
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
        cfg.report.output_path.clone_from(o);
    }
    Ok(cfg)
}

/// 校验时间字符串格式（绝对时间或相对时间表达式）。
fn validate_time_or_expr(s: &str) -> Result<(), AppError> {
    // 先尝试绝对时间
    if NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").is_ok() {
        return Ok(());
    }
    // 再检查是否为合法的相对时间表达式
    if crate::time_expr::is_relative_time(s) {
        return Ok(());
    }
    Err(AppError::TimeFormat {
        raw: format!(
            "「{s}」既不是绝对时间（YYYY-MM-DD HH:MM:SS）也不是相对时间表达式（now/start/end[+/-N单位]）"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_yaml_round_trips() {
        let yaml = default_config_yaml();
        let cfg: AppConfig = serde_yaml_ng::from_str(&yaml).expect("默认 YAML 必须可解析");
        assert_eq!(cfg.devices.get("nvidia_a10").unwrap().card_id_label, "gpu");
        assert_eq!(cfg.devices.get("ascend_910b").unwrap().card_id_label, "id");
        assert!(cfg.thresholds.core_avg_above.is_none()); // 默认模板里 thresholds 全为 null
    }

    #[test]
    fn apply_overrides_requires_both_start_and_end() {
        let cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
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
        let cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
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
        assert!(validate_time_or_expr("2026/01/01 00:00:00").is_err());
        assert!(validate_time_or_expr("2026-01-01 00:00:00").is_ok());
        assert!(validate_time_or_expr("now-7d").is_ok());
        assert!(validate_time_or_expr("start+3h").is_ok());
        assert!(validate_time_or_expr("tomorrow").is_err());
        // is_relative_time 严格检查：锚点后跟非偏移字符不应通过
        assert!(validate_time_or_expr("nowhere").is_err());
        assert!(validate_time_or_expr("starting_point").is_err());
    }

    #[test]
    fn config_rejects_zero_query_step() {
        let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        cfg.report.query_step_secs = 0;
        assert!(validate_config(&cfg, "test.yaml").is_err());
    }

    #[test]
    fn config_rejects_start_ge_end() {
        let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        cfg.time_range.start = "2026-06-19 00:00:00".into();
        cfg.time_range.end = "2026-06-18 00:00:00".into();
        assert!(validate_config(&cfg, "test.yaml").is_err());
    }

    #[test]
    fn config_accepts_valid_time_range() {
        let cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        assert!(validate_config(&cfg, "test.yaml").is_ok());
    }

    #[test]
    fn apply_overrides_accepts_absolute_and_relative_times() {
        // 绝对时间
        let cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        let out = apply_overrides(
            cfg,
            &CliOverrides {
                start: Some("2026-01-01 00:00:00".into()),
                end: Some("2026-01-02 00:00:00".into()),
                config_path: None,
                output: None,
            },
        )
        .unwrap();
        assert_eq!(out.time_range.start, "2026-01-01 00:00:00");

        // 相对时间表达式
        let cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        let out = apply_overrides(
            cfg,
            &CliOverrides {
                start: Some("now-7d".into()),
                end: Some("now".into()),
                config_path: None,
                output: None,
            },
        )
        .unwrap();
        assert_eq!(out.time_range.start, "now-7d");
        assert_eq!(out.time_range.end, "now");
    }

    #[test]
    fn config_rejects_zero_timeout() {
        let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        cfg.sources[0].timeout_secs = 0;
        assert!(
            validate_config(&cfg, "test.yaml").is_err(),
            "timeout_secs=0 应被拒绝"
        );
    }

    #[test]
    fn config_rejects_oversized_query_step() {
        let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        cfg.report.query_step_secs = u64::MAX;
        assert!(
            validate_config(&cfg, "test.yaml").is_err(),
            "超大 query_step_secs 应被拒绝"
        );
    }

    #[test]
    fn config_rejects_empty_sources() {
        let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        cfg.sources.clear();
        assert!(
            validate_config(&cfg, "test.yaml").is_err(),
            "空 sources 应被拒绝"
        );
    }

    #[test]
    fn config_rejects_url_without_scheme() {
        let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        cfg.sources[0].url = "192.168.1.100:9090".into();
        let r = validate_config(&cfg, "test.yaml");
        assert!(r.is_err(), "无协议前缀的 URL 应被拒绝");
        let msg = format!("{}", r.unwrap_err());
        assert!(
            msg.contains("http://") || msg.contains("https://"),
            "提示应含协议要求"
        );
    }

    #[test]
    fn config_rejects_invalid_metric_name() {
        let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        // 注入含 PromQL 特殊字符的指标名
        cfg.devices.get_mut("nvidia_a10").unwrap().core_util_metric = "metric{evil=\"yes\"}".into();
        let r = validate_config(&cfg, "test.yaml");
        assert!(r.is_err(), "含特殊字符的指标名应被拒绝");
    }

    #[test]
    fn config_rejects_invalid_label_name() {
        let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        cfg.devices.get_mut("nvidia_a10").unwrap().card_id_label = "gpu\",foo=\"bar".into();
        let r = validate_config(&cfg, "test.yaml");
        assert!(r.is_err(), "含特殊字符的标签名应被拒绝");
    }

    #[test]
    fn config_rejects_undefined_device_type() {
        let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        cfg.sources[0]
            .device_types
            .push("nonexistent_device".into());
        let r = validate_config(&cfg, "test.yaml");
        assert!(r.is_err(), "引用未定义的设备类型应被拒绝");
        let msg = format!("{}", r.unwrap_err());
        assert!(
            msg.contains("nonexistent_device"),
            "错误信息应包含设备类型名"
        );
    }

    #[test]
    fn config_accepts_valid_metric_and_label_names() {
        // 默认配置的指标名/标签名都应通过校验
        let cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        assert!(validate_config(&cfg, "test.yaml").is_ok());
    }

    #[test]
    fn is_valid_metric_name_accepts_standard_names() {
        assert!(is_valid_metric_name("DCGM_FI_DEV_GPU_UTIL"));
        assert!(is_valid_metric_name("npu_chip_info_utilization"));
        assert!(is_valid_metric_name(":metric:with:colons:"));
        assert!(is_valid_metric_name("_starts_with_underscore"));
        assert!(!is_valid_metric_name("")); // 空
        assert!(!is_valid_metric_name("1starts_with_digit"));
        assert!(!is_valid_metric_name("metric with space"));
        assert!(!is_valid_metric_name("metric{evil}"));
    }

    #[test]
    fn is_valid_label_name_accepts_standard_names() {
        assert!(is_valid_label_name("gpu"));
        assert!(is_valid_label_name("container_name"));
        assert!(is_valid_label_name("_private"));
        assert!(!is_valid_label_name("")); // 空
        assert!(!is_valid_label_name("1digit"));
        assert!(!is_valid_label_name("label:colon")); // 冒号不允许在标签名中
        assert!(!is_valid_label_name("label\"quote"));
    }

    #[test]
    fn config_rejects_mapping_with_unknown_record_key() {
        let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        cfg.mapping = Some(crate::mapper::MappingConfig {
            enabled: true,
            sources: vec![crate::mapper::MappingSource {
                source_path: "assets.csv".into(),
                source_sheet: None,
                match_keys: "IP地址".into(),
                record_key: Some("unknown_field".into()),
                columns: vec![],
            }],
        });
        let r = validate_config(&cfg, "test.yaml");
        assert!(r.is_err(), "未知 record_key 应被拒绝");
        let msg = format!("{}", r.unwrap_err());
        assert!(msg.contains("unknown_field"), "错误信息应包含字段名");
    }

    #[test]
    fn config_rejects_mapping_with_empty_match_keys() {
        let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        cfg.mapping = Some(crate::mapper::MappingConfig {
            enabled: true,
            sources: vec![crate::mapper::MappingSource {
                source_path: "assets.csv".into(),
                source_sheet: None,
                match_keys: String::new(),
                record_key: None,
                columns: vec![],
            }],
        });
        let r = validate_config(&cfg, "test.yaml");
        assert!(r.is_err(), "空 match_keys 应被拒绝");
    }

    #[test]
    fn config_accepts_mapping_with_known_record_key() {
        let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        cfg.mapping = Some(crate::mapper::MappingConfig {
            enabled: true,
            sources: vec![crate::mapper::MappingSource {
                source_path: "assets.csv".into(),
                source_sheet: None,
                match_keys: "IP地址".into(),
                record_key: Some("host_ip".into()),
                columns: vec![],
            }],
        });
        assert!(
            validate_config(&cfg, "test.yaml").is_ok(),
            "已知 record_key 应通过校验"
        );
    }

    #[test]
    fn config_accepts_valid_timezone() {
        let cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        assert_eq!(cfg.timezone, "Asia/Shanghai");
        assert!(validate_config(&cfg, "test.yaml").is_ok());
    }

    #[test]
    fn config_rejects_invalid_timezone() {
        let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        cfg.timezone = "Invalid/Zone".into();
        let r = validate_config(&cfg, "test.yaml");
        assert!(r.is_err(), "非法时区名应被拒绝");
    }

    #[test]
    fn config_rejects_deeply_nested_fallback() {
        // 构造超过 MAX_FALLBACK_DEPTH(10) 层的 DirectMetric 嵌套链
        use crate::devices::{DirectInner, DirectMetricBody, MemoryStrategy};
        let mut inner =
            MemoryStrategy::CompositeFromTotal(crate::devices::CompositeFromTotalBody {
                composite_from_total: crate::devices::UsedTotal {
                    used: "bottom_used".into(),
                    total: "bottom_total".into(),
                },
            });
        for _ in 0..11 {
            inner = MemoryStrategy::DirectMetric(DirectMetricBody {
                direct_metric: DirectInner {
                    metric: "deep_metric".into(),
                    fallback: Some(Box::new(inner)),
                },
            });
        }
        let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
        cfg.devices.get_mut("nvidia_a10").unwrap().memory = inner;
        let r = validate_config(&cfg, "test.yaml");
        assert!(r.is_err(), "超过 10 层的 fallback 嵌套应被拒绝");
        let msg = format!("{}", r.unwrap_err());
        assert!(
            msg.contains("fallback") || msg.contains("嵌套"),
            "错误信息应提及 fallback 嵌套"
        );
    }
}
