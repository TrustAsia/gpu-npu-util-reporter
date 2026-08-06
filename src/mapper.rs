//! 资产映射引擎模块。
//!
//! 职责二合一：(1) 加载外部资产表（CSV/Excel/MySQL）并按 `match_keys` 与每行
//! [`CardRecord`] 做 Join，注入资产字段；(2) 计算映射列在报表中的最终位置
//! （锚点列 + before/after 方向）。开关关闭时整个模块跳过。
//!
//! Join 设计：加载阶段为每行资产注入一个隐藏列 `@key`（由 `match_keys` 指定的
//! 资产列值以分隔符拼接），join 时把 `CardRecord` 同样字段拼成 key 直接比对，
//! 精确模式 O(行数) 哈希查找。
//!
//! 支持多来源映射：每个 `MappingSource` 可指定独立的资产表路径、匹配键和列映射，
//! 允许从不同资产表分别取值注入报表。
//!
//! # 匹配能力（1.10.0 新增）
//!
//! - **多键**：`match_keys` 为字符串列表，多键之间 AND 组合（全部命中才注入）。
//! - **正则**：`match_mode: exact|regex`。regex 模式下正则按**全值锚定**匹配
//!   （自动包裹 `^...$`），避免 `10.0.1.5` 命中 `210.0.1.55` 之类的子串误配。
//!   不含通配符元字符（`*+?()[]{}^$|\`）的模式按字面量精确匹配，走哈希索引。
//! - **方向**：`match_direction: asset_pattern|record_pattern`。
//!   - `asset_pattern`：资产侧列值是模式，匹配 CardRecord 的值（资产表是"规则库"）。
//!   - `record_pattern`：CardRecord 的值是模式，匹配资产侧字面量（少见）。
//! - **多行命中**：正则模式下一行记录可能命中多行资产，取首行并记 Warning。
//!
//! `match_keys` 为字符串列表，直接指定资产表中的列名。CardRecord 侧通过
//! `record_key`（可选列表，与 match_keys 一一对应）指定对应字段名；不指定时
//! 默认与 `match_keys` 相同。[`card_record_field`] 支持的字段名：`source_name`、
//! `host_ip`、`node_name`、`card_id`、`device_type`、`namespace`、`pod`、
//! `container`。不在上述列表中的字段名在 `CardRecord` 侧取值为空串（join 不命中）。
//!
//! # 数据来源（1.10.0 新增）
//!
//! `source_type: file|mysql`。file 走 CSV/Excel；mysql 配置连接信息 + `table` 表名，
//! 程序自动生成 `SELECT * FROM \`table\``（不手写 SQL），文本协议读取，列顺序无关。

use crate::error::AppError;
use crate::processor::CardRecord;
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::{HashMap, HashSet};

/// 报表核心基础列（始终出现）。
const CORE_BASE_COLUMNS: &[&str] = &[
    "数据来源",
    "数据开始时间",
    "数据结束时间",
    "主机IP",
    "节点名称",
    "计算卡编号",
    "设备类型",
    "Namespace",
    "Pod",
    "容器名称",
    "取值时间范围",
    "核心利用率平均值",
    "核心利用率峰值",
    "核心利用率峰值出现时间",
    "核心利用率数据量",
    "核心利用率首条数据时间",
    "核心利用率末条数据时间",
    "显存占用率平均值",
    "显存占用率峰值",
    "显存占用率峰值出现时间",
    "显存占用率数据量",
    "显存占用率首条数据时间",
    "显存占用率末条数据时间",
];

/// 核心基础列对应的本地字段名（与 CORE_BASE_COLUMNS 一一对应）。
///
/// 本地字段名是列的稳定标识符，用于配置映射到数据库列名和字段类型，
/// 不受报表显示名变化影响。
pub const CORE_BASE_LOCAL_NAMES: &[&str] = &[
    "source_name",
    "range_start",
    "range_end",
    "host_ip",
    "node_name",
    "card_id",
    "device_type",
    "namespace",
    "pod",
    "container",
    "time_range",
    "core_avg",
    "core_peak",
    "core_peak_time",
    "core_count",
    "core_first_time",
    "core_last_time",
    "mem_avg",
    "mem_peak",
    "mem_peak_time",
    "mem_count",
    "mem_first_time",
    "mem_last_time",
];

/// 设备温度列（按温度指标配置时出现）。
pub const TEMP_COLUMNS: &[&str] = &[
    "设备温度平均值",
    "设备温度峰值",
    "设备温度峰值出现时间",
    "设备温度数据量",
    "设备温度首条数据时间",
    "设备温度末条数据时间",
];

/// 设备温度列对应的本地字段名。
pub const TEMP_LOCAL_NAMES: &[&str] = &[
    "temp_avg",
    "temp_peak",
    "temp_peak_time",
    "temp_count",
    "temp_first_time",
    "temp_last_time",
];

/// 设备功率列（按功率指标配置时出现）。
pub const POWER_COLUMNS: &[&str] = &[
    "设备功率平均值",
    "设备功率峰值",
    "设备功率峰值出现时间",
    "设备功率数据量",
    "设备功率首条数据时间",
    "设备功率末条数据时间",
];

/// 设备功率列对应的本地字段名。
pub const POWER_LOCAL_NAMES: &[&str] = &[
    "power_avg",
    "power_peak",
    "power_peak_time",
    "power_count",
    "power_first_time",
    "power_last_time",
];

/// 主机 CPU 列（启用主机指标时出现）。
pub const HOST_CPU_COLUMNS: &[&str] = &[
    "主机CPU利用率平均值",
    "主机CPU利用率峰值",
    "主机CPU利用率峰值出现时间",
];

/// 主机 CPU 列对应的本地字段名。
pub const HOST_CPU_LOCAL_NAMES: &[&str] = &[
    "host_cpu_avg",
    "host_cpu_peak",
    "host_cpu_peak_time",
];

/// 主机内存列（启用主机指标时出现）。
pub const HOST_MEM_COLUMNS: &[&str] = &[
    "主机内存利用率平均值",
    "主机内存利用率峰值",
    "主机内存利用率峰值出现时间",
];

/// 主机内存列对应的本地字段名。
pub const HOST_MEM_LOCAL_NAMES: &[&str] = &[
    "host_mem_avg",
    "host_mem_peak",
    "host_mem_peak_time",
];

/// 主机句柄数列（启用主机指标且配置了 handle_expr 时出现）。
pub const HOST_HANDLE_COLUMNS: &[&str] = &[
    "主机句柄数平均值",
    "主机句柄数峰值",
    "主机句柄数峰值出现时间",
];

/// 主机句柄数列对应的本地字段名。
pub const HOST_HANDLE_LOCAL_NAMES: &[&str] = &[
    "host_handle_avg",
    "host_handle_peak",
    "host_handle_peak_time",
];

/// 标志位：哪些可选指标组应出现在基础列中。
#[derive(Debug, Clone, Copy, Default)]
pub struct ColumnFlags {
    pub has_temp: bool,
    pub has_power: bool,
    pub has_host_cpu: bool,
    pub has_host_mem: bool,
    pub has_host_handle: bool,
}

/// 根据设备配方计算列标志位。
///
/// 仅检查被数据源实际引用的设备类型，避免未引用的设备类型
/// 在报表中产生全 N/A 的额外列。
/// 主机指标从设备配方的 `host_metrics` 字段获取（而非全局配置）。
#[must_use]
pub fn compute_column_flags(
    sources: &[crate::config::SourceConfig],
    devices: &std::collections::HashMap<String, crate::devices::DeviceSpec>,
) -> ColumnFlags {
    let mut flags = ColumnFlags::default();
    let active_device_keys: std::collections::HashSet<&String> =
        sources.iter().flat_map(|s| &s.device_types).collect();
    for (key, spec) in devices {
        if active_device_keys.contains(key) {
            if spec.temp_metric.is_some() {
                flags.has_temp = true;
            }
            if spec.power_metric.is_some() {
                flags.has_power = true;
            }
            if let Some(hm) = &spec.host_metrics {
                if hm.enabled {
                    flags.has_host_cpu |= hm.cpu_expr.is_some();
                    flags.has_host_mem |= hm.mem_expr.is_some();
                    flags.has_host_handle |= hm.handle_expr.is_some();
                }
            }
        }
    }
    flags
}

/// 根据配置构建基础列有序清单。
///
/// 核心列始终出现；可选列组按 flags 决定是否追加。
#[must_use]
pub fn build_base_columns(flags: ColumnFlags) -> Vec<String> {
    build_base_pairs(flags).into_iter().map(|(display, _)| display).collect()
}

/// 根据配置构建基础列本地字段名有序清单（与 [`build_base_columns`] 一一对应）。
#[must_use]
pub fn build_base_local_names(flags: ColumnFlags) -> Vec<String> {
    build_base_pairs(flags).into_iter().map(|(_, local)| local).collect()
}

/// 一次性构建 (显示名, 本地字段名) 配对有序清单，避免两个独立函数不同步。
#[must_use]
fn build_base_pairs(flags: ColumnFlags) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = CORE_BASE_COLUMNS
        .iter()
        .zip(CORE_BASE_LOCAL_NAMES.iter())
        .map(|(&d, &l)| (d.to_string(), l.to_string()))
        .collect();
    if flags.has_temp {
        pairs.extend(
            TEMP_COLUMNS
                .iter()
                .zip(TEMP_LOCAL_NAMES.iter())
                .map(|(&d, &l)| (d.to_string(), l.to_string())),
        );
    }
    if flags.has_power {
        pairs.extend(
            POWER_COLUMNS
                .iter()
                .zip(POWER_LOCAL_NAMES.iter())
                .map(|(&d, &l)| (d.to_string(), l.to_string())),
        );
    }
    if flags.has_host_cpu {
        pairs.extend(
            HOST_CPU_COLUMNS
                .iter()
                .zip(HOST_CPU_LOCAL_NAMES.iter())
                .map(|(&d, &l)| (d.to_string(), l.to_string())),
        );
    }
    if flags.has_host_mem {
        pairs.extend(
            HOST_MEM_COLUMNS
                .iter()
                .zip(HOST_MEM_LOCAL_NAMES.iter())
                .map(|(&d, &l)| (d.to_string(), l.to_string())),
        );
    }
    if flags.has_host_handle {
        pairs.extend(
            HOST_HANDLE_COLUMNS
                .iter()
                .zip(HOST_HANDLE_LOCAL_NAMES.iter())
                .map(|(&d, &l)| (d.to_string(), l.to_string())),
        );
    }
    pairs
}

/// 根据报表显示列名查找对应的本地字段名。
///
/// 基础列通过预定义映射查找；映射列通过 `MappingColumn.local_name` 查找。
/// 未找到时返回 `None`。
#[must_use]
pub fn local_name_for_column(
    display_name: &str,
    base_columns: &[String],
    base_local_names: &[String],
    mapping_columns: &[MappingColumn],
) -> Option<String> {
    // 先在基础列中查找
    if let Some(idx) = base_columns.iter().position(|c| c == display_name) {
        return base_local_names.get(idx).cloned();
    }
    // 再在映射列中查找
    mapping_columns
        .iter()
        .find(|c| c.rename == display_name)
        .map(|c| c.effective_local_name().to_string())
}

/// 向后兼容：默认基础列（仅核心列，不含可选指标组）。
pub const BASE_COLUMNS: &[&str] = CORE_BASE_COLUMNS;

/// 列插入位置：相对于某锚点列的前/后。
///
/// serde 表示为一个对象 `{ direction: before|after, anchor: <列名> }`，
/// 而非外部标记枚举——因为 `serde_yaml_ng` 不支持默认的 externally-tagged 变体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InsertPosition {
    /// 方向：`before` 或 `after`。
    pub direction: Direction,
    /// 锚点列名（必须为基础列）。
    pub anchor: String,
}

/// 插入方向。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Before,
    After,
}

impl InsertPosition {
    /// 便捷构造：锚点列之前。
    pub fn before(anchor: impl Into<String>) -> Self {
        Self {
            direction: Direction::Before,
            anchor: anchor.into(),
        }
    }
    /// 便捷构造：锚点列之后。
    pub fn after(anchor: impl Into<String>) -> Self {
        Self {
            direction: Direction::After,
            anchor: anchor.into(),
        }
    }
}

/// 单个映射列的配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MappingColumn {
    /// 资产表源列名。
    pub source_field: String,
    /// 注入后的新列名（报表显示名）。
    pub rename: String,
    /// 本地字段名（稳定标识符，用于映射到数据库列名，不受显示名变化影响）。
    /// 不指定时默认与 `source_field` 相同。
    #[serde(default)]
    pub local_name: Option<String>,
    /// 插入位置。
    pub position: InsertPosition,
}

/// 单个映射来源：独立的资产表（文件/MySQL）+ 匹配键 + 列映射。
///
/// `match_keys` 为字符串列表（兼容旧配置的单字符串写法），指定资产表中的
/// 匹配列名，多键之间 AND 组合。CardRecord 侧通过 `record_key`（可选，与
/// match_keys 一一对应）指定对应字段名；不指定时默认与 `match_keys` 相同。
/// [`card_record_field`] 支持的字段名：`source_name`、`host_ip`、`node_name`、
/// `card_id`、`device_type`、`namespace`、`pod`、`container`。
/// 不在已知列表中的字段名在 `CardRecord` 侧取值为空串。
///
/// `match_mode: exact|regex` 与 `match_direction: asset_pattern|record_pattern`
/// 作用于本来源的所有匹配键（1.10.0 新增）。
///
/// `source_type: file|mysql`。file 时使用 `source_path`（可选 `source_sheet`
/// 指定 Excel 工作表名，不指定取第一个工作表）；mysql 时使用连接字段 + `table`
/// 表名，程序自动生成 `SELECT *` 查询，不手写 SQL。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MappingSource {
    /// 数据来源类型：`file`（CSV/Excel）或 `mysql`。默认 `file`。
    #[serde(default = "default_source_type")]
    pub source_type: String,
    /// 资产表路径（`source_type: file` 时使用，按扩展名分流 CSV/Excel）。
    pub source_path: String,
    /// 可选 Excel 工作表名；不指定时取第一个工作表。
    #[serde(default)]
    pub source_sheet: Option<String>,
    /// MySQL 连接：主机地址（`source_type: mysql` 时必填）。
    #[serde(default)]
    pub host: Option<String>,
    /// MySQL 连接：端口（默认 3306）。
    #[serde(default)]
    pub port: Option<u16>,
    /// MySQL 连接：用户名（必填）。
    #[serde(default)]
    pub username: Option<String>,
    /// MySQL 连接：密码。
    #[serde(default)]
    pub password: Option<String>,
    /// MySQL 连接：数据库名（必填）。
    #[serde(default)]
    pub database: Option<String>,
    /// MySQL 资产表名（必填）；程序自动执行 `SELECT * FROM \`table\``。
    #[serde(default)]
    pub table: Option<String>,
    /// 资产表中的匹配列名列表（多键 AND 组合）。
    ///
    /// 兼容旧配置的单字符串写法（如 `match_keys: "host_ip"`）。
    /// `CardRecord` 侧通过 `record_key` 映射对应字段；不指定 `record_key` 时
    /// 默认与 `match_keys` 相同。
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub match_keys: Vec<String>,
    /// `CardRecord` 侧对应的字段名列表（可选，与 `match_keys` 一一对应）。
    ///
    /// 支持的字段名：`source_name`、`host_ip`、`node_name`、`card_id`、
    /// `device_type`、`namespace`、`pod`、`container`。
    /// 兼容旧配置的单字符串写法（如 `record_key: "host_ip"`）。
    /// 不指定时默认与 `match_keys` 相同，适用于资产表列名与 `CardRecord`
    /// 字段名一致的场景（如 `host_ip`）。
    /// 当资产表列名不同于 `CardRecord` 字段名时（如资产表用 `IP地址`，
    /// `CardRecord` 用 `host_ip`），需要显式指定 `record_key`。
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub record_key: Vec<String>,
    /// 匹配模式：`exact`（精确匹配，默认）或 `regex`（正则全值锚定匹配）。
    #[serde(default = "default_match_mode")]
    pub match_mode: String,
    /// 正则匹配方向：`asset_pattern`（资产侧列值是模式，默认）或
    /// `record_pattern`（记录侧值是模式）。仅 `match_mode: regex` 时生效。
    #[serde(default = "default_match_direction")]
    pub match_direction: String,
    /// 从该资产表提取的列映射。
    pub columns: Vec<MappingColumn>,
}

fn default_source_type() -> String {
    "file".into()
}

fn default_match_mode() -> String {
    "exact".into()
}

fn default_match_direction() -> String {
    "asset_pattern".into()
}

impl Default for MappingSource {
    fn default() -> Self {
        Self {
            source_type: default_source_type(),
            source_path: String::new(),
            source_sheet: None,
            host: None,
            port: None,
            username: None,
            password: None,
            database: None,
            table: None,
            match_keys: Vec::new(),
            record_key: Vec::new(),
            match_mode: default_match_mode(),
            match_direction: default_match_direction(),
            columns: Vec::new(),
        }
    }
}

/// 反序列化辅助：接受单个字符串或字符串列表（`match_keys`/`record_key` 的
/// 旧配置兼容写法是单字符串，新写法是列表）。
fn deserialize_string_or_vec<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

/// 匹配模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    Exact,
    Regex,
}

/// 正则匹配方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchDirection {
    /// 资产侧列值是正则模式，匹配记录侧字面量值。
    AssetPattern,
    /// 记录侧值是正则模式，匹配资产侧字面量值。
    RecordPattern,
}

impl MappingSource {
    /// 解析 `match_mode` 配置字符串；非法值返回错误。
    pub fn parse_match_mode(&self) -> Result<MatchMode, AppError> {
        match self.match_mode.as_str() {
            "exact" => Ok(MatchMode::Exact),
            "regex" => Ok(MatchMode::Regex),
            other => Err(AppError::Mapping {
                path: self.source_path.clone(),
                detail: format!(
                    "match_mode「{other}」不支持（仅支持 exact / regex）"
                ),
            }),
        }
    }

    /// 解析 `match_direction` 配置字符串；非法值返回错误。
    pub fn parse_match_direction(&self) -> Result<MatchDirection, AppError> {
        match self.match_direction.as_str() {
            "asset_pattern" => Ok(MatchDirection::AssetPattern),
            "record_pattern" => Ok(MatchDirection::RecordPattern),
            other => Err(AppError::Mapping {
                path: self.source_path.clone(),
                detail: format!(
                    "match_direction「{other}」不支持（仅支持 asset_pattern / record_pattern）"
                ),
            }),
        }
    }

    /// 计算 CardRecord 侧的字段名列表：显式配置 `record_key` 时使用之
    /// （长度须与 `match_keys` 一致），否则默认与 `match_keys` 相同。
    fn record_field_names(&self) -> Result<Vec<String>, AppError> {
        if self.record_key.is_empty() {
            Ok(self.match_keys.clone())
        } else if self.record_key.len() == self.match_keys.len() {
            Ok(self.record_key.clone())
        } else {
            Err(AppError::Mapping {
                path: self.source_path.clone(),
                detail: format!(
                    "record_key（{} 个）与 match_keys（{} 个）数量不一致，多键匹配时二者必须一一对应",
                    self.record_key.len(),
                    self.match_keys.len()
                ),
            })
        }
    }
}

/// 资产映射总配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MappingConfig {
    pub enabled: bool,
    /// 多来源映射列表，每个来源可指定独立的资产表、匹配键和列映射。
    pub sources: Vec<MappingSource>,
}

impl MappingConfig {
    /// 收集所有来源的映射列（owned clone），用于需要所有权的场景。
    #[must_use]
    pub fn all_columns_owned(&self) -> Vec<MappingColumn> {
        self.sources
            .iter()
            .flat_map(|s| s.columns.clone())
            .collect()
    }

    /// 检测所有来源中是否存在重复的 rename，返回警告列表。
    /// 重复 rename 会导致 Excel 列名重复和数据覆盖，应在配置阶段拒绝。
    #[must_use]
    pub fn duplicate_rename_warnings(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut dupes = Vec::new();
        for col in self.sources.iter().flat_map(|s| &s.columns) {
            if !seen.insert(&col.rename) {
                dupes.push(col.rename.clone());
            }
        }
        dupes.sort();
        dupes.dedup();
        dupes
            .into_iter()
            .map(|r| format!("映射列 rename「{r}」在多个来源中重复，将导致数据覆盖"))
            .collect()
    }

    /// 检测所有来源中是否存在重复的 local_name，返回警告列表。
    /// 重复 local_name 会导致数据库映射歧义，应在配置阶段拒绝。
    #[must_use]
    pub fn duplicate_local_name_warnings(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut dupes = Vec::new();
        for col in self.sources.iter().flat_map(|s| &s.columns) {
            let ln = col.effective_local_name();
            if !seen.insert(ln) {
                dupes.push(ln.to_string());
            }
        }
        dupes.sort();
        dupes.dedup();
        dupes
            .into_iter()
            .map(|r| format!("映射列 local_name「{r}」在多个来源中重复，将导致数据库映射歧义"))
            .collect()
    }

    /// 检测映射列 rename 是否与当前活跃的基础列显示名冲突。
    /// 仅检查由 flags 决定的活跃列，避免对未启用的指标组过度拒绝。
    /// 冲突会导致 compute_column_order 产出重复列名，造成 Excel/数据库数据错乱。
    #[must_use]
    pub fn rename_collides_with_base_warnings(&self, flags: ColumnFlags) -> Vec<String> {
        let active_base = build_base_columns(flags);
        let active_set: std::collections::HashSet<&str> =
            active_base.iter().map(String::as_str).collect();
        self.sources
            .iter()
            .flat_map(|s| &s.columns)
            .filter(|c| active_set.contains(c.rename.as_str()))
            .map(|c| {
                format!(
                    "映射列 rename「{}」与基础列显示名冲突，将导致数据覆盖",
                    c.rename
                )
            })
            .collect()
    }
}

impl MappingColumn {
    /// 获取有效的本地字段名：显式配置时使用 `local_name`，否则回退到 `source_field`。
    /// 返回借用以避免不必要的堆分配。
    #[must_use]
    pub fn effective_local_name(&self) -> &str {
        self.local_name
            .as_deref()
            .unwrap_or(&self.source_field)
    }
}

/// 资产表行：列名 → 值（含加载阶段注入的 `@key`）。
type AssetRow = HashMap<String, String>;

/// `CardRecord` 已知字段名列表，用于校验 `record_key` / `match_keys` 配置。
pub const KNOWN_CARD_RECORD_FIELDS: &[&str] = &[
    "source_name",
    "host_ip",
    "node_name",
    "card_id",
    "device_type",
    "namespace",
    "pod",
    "container",
];

/// `CardRecord` 已知字段名 → 字段值映射。
///
/// 支持的字段名：`source_name`、`host_ip`、`node_name`、`card_id`、
/// `device_type`、`namespace`、`pod`、`container`。
/// 不在上述列表中的字段名返回空串。
#[must_use]
pub fn card_record_field(rec: &CardRecord, field: &str) -> String {
    match field {
        "source_name" => rec.source_name.clone(),
        "host_ip" => rec.host_ip.clone(),
        "node_name" => rec.node_name.clone(),
        "card_id" => rec.card_id.clone(),
        "device_type" => rec.device_type.clone(),
        "namespace" => rec.namespace.clone(),
        "pod" => rec.pod.clone(),
        "container" => rec.container.clone(),
        _ => String::new(),
    }
}

/// 多键组合分隔符：拼入 `@key` 的分隔字符，避免多键值互相粘连。
/// 实际数据（IP/主机名/卡号等）不会包含控制字符 \x1F，不会误撞。
const KEY_SEPARATOR: char = '\u{1F}';

/// 由多个字段值拼成组合键字符串（多键 AND 匹配的精确键）。
fn build_combined_key(values: impl IntoIterator<Item = String>) -> String {
    values
        .into_iter()
        .collect::<Vec<_>>()
        .join(&KEY_SEPARATOR.to_string())
}

/// 为一行资产注入 `@key`（由 `match_keys` 指定的各列值以分隔符拼接）。
fn inject_keys(row: &mut AssetRow, match_keys: &[String]) {
    let key = build_combined_key(
        match_keys
            .iter()
            .map(|k| row.get(k).cloned().unwrap_or_default()),
    );
    row.insert("@key".into(), key);
}

/// 由一张卡构造 join key 字符串（多键组合）。
/// 使用 `record_field_names` 解析出的字段名列表逐键取值。
fn build_record_key(rec: &CardRecord, record_fields: &[String]) -> String {
    build_combined_key(record_fields.iter().map(|f| card_record_field(rec, f)))
}

/// 计算最终列顺序：基础列 + 按 position 插入的映射列。
///
/// 算法：每个 `MappingColumn` 解析出目标 index（Before(X)→X 的 index，
/// After(X)→X 的 index + 1）。**位置锚点 X 必须是基础列之一**（PRD §2.3
/// 锚点约束）——不允许以其它映射列为锚点。因此所有目标 index 由基础列布局
/// 唯一确定、互不影响，一次性计算即可。按 index 升序、同 index 按 config
/// 顺序从后往前插入到 `result`（保持同 index 列按配置顺序堆叠）。
/// 锚点不在基础列中时该列追加到末尾。
/// 检测锚点不在基础列中的映射列，返回对应的 Warning 消息（PRD §2.3）。
///
/// PRD §2.3 锚点约束：映射列的位置锚点必须是基础列之一；否则记 Warning 并把
/// 该列追加到末尾（追加行为在 [`compute_column_order`] 中实现）。本函数只负责
/// 产出 Warning 文本，由 main 统一收集打印，便于单元测试。
#[must_use]
pub fn missing_anchor_warnings(base: &[&str], mapping_cols: &[MappingColumn]) -> Vec<String> {
    mapping_cols
        .iter()
        .filter(|c| !base.iter().any(|b| *b == c.position.anchor))
        .map(|c| {
            format!(
                "[警告] 映射列「{}」的锚点「{}」不是基础列，已追加到末尾",
                c.rename, c.position.anchor
            )
        })
        .collect()
}

#[must_use]
pub fn compute_column_order(base: &[&str], mapping_cols: &[MappingColumn]) -> Vec<String> {
    let mut result: Vec<String> = base.iter().map(ToString::to_string).collect();
    // 目标 index 仅取决于基础列（锚点被约束为基础列），互不影响
    // 缺失锚点的列追加到末尾：target 需大于所有有效锚点的 target，
    // 否则与指向末尾基础列 After 的有效锚点列 target 相同时会交错插入。
    let missing_target = base.len() + mapping_cols.len();
    let mut placements: Vec<(usize, String)> = mapping_cols
        .iter()
        .map(|c| {
            let target = base.iter().position(|x| *x == c.position.anchor).map_or(
                missing_target,
                |idx| match c.position.direction {
                    Direction::Before => idx,
                    Direction::After => idx + 1,
                },
            );
            (target, c.rename.clone())
        })
        .collect();
    // 稳定排序后从后往前插入：同 index 的多列按配置顺序堆叠
    placements.sort_by_key(|(idx, _)| *idx);
    for (target, rename) in placements.into_iter().rev() {
        let insert_at = target.min(result.len());
        result.insert(insert_at, rename);
    }
    result
}

/// 资产表行数上限，防止异常大的资产文件耗尽内存。
const MAX_ASSET_ROWS: usize = 1_000_000;

/// 加载文件型资产表（CSV/Excel），并为每行注入 `@key`（由 `match_keys`
/// 指定的各列拼成）。按扩展名分流：`.csv` 用 csv crate，
/// `.xlsx`/`.xls`/`.xlsb`/`.ods` 用 calamine 自动检测。首行视为表头。
///
/// # Errors
///
/// 返回 [`AppError::Mapping`] 当文件读取/解析失败、格式不支持或匹配列缺失。
pub fn load_asset_table(source: &MappingSource) -> Result<Vec<AssetRow>, AppError> {
    let path = &source.source_path;
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "csv" {
        load_csv(path, &source.match_keys)
    } else if matches!(ext.as_str(), "xlsx" | "xls" | "xlsb" | "ods") {
        load_excel(path, &source.match_keys, source.source_sheet.as_deref())
    } else {
        Err(AppError::Mapping {
            path: path.into(),
            detail: "不支持的资产表格式（仅支持 .csv/.xlsx/.xls/.xlsb/.ods）".into(),
        })
    }
}

/// 检查 `match_keys` 各列是否存在于表头中，缺失时返回错误并列出可用列。
fn validate_match_key_in_headers(
    headers: &[String],
    match_keys: &[String],
    path: &str,
) -> Result<(), AppError> {
    if match_keys.is_empty() {
        return Err(AppError::Mapping {
            path: path.into(),
            detail: "match_keys 不能为空列表".into(),
        });
    }
    if let Some(_key) = match_keys.iter().find(|k| k.is_empty()) {
        return Err(AppError::Mapping {
            path: path.into(),
            detail: "match_keys 不能包含空字符串".into(),
        });
    }
    let missing: Vec<&String> = match_keys
        .iter()
        .filter(|k| !headers.iter().any(|h| h == *k))
        .collect();
    if !missing.is_empty() {
        return Err(AppError::Mapping {
            path: path.into(),
            detail: format!(
                "match_keys「{}」在资产表表头中不存在（可用列：{}）",
                missing
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("、"),
                headers.join(", ")
            ),
        });
    }
    Ok(())
}

fn load_csv(path: &str, match_keys: &[String]) -> Result<Vec<AssetRow>, AppError> {
    let content = std::fs::read_to_string(path).map_err(|e| AppError::Mapping {
        path: path.into(),
        detail: format!("读取失败：{e}"),
    })?;
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .from_reader(content.as_bytes());
    let headers = rdr
        .headers()
        .map_err(|e| AppError::Mapping {
            path: path.into(),
            detail: format!("解析表头失败：{e}"),
        })?
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    validate_match_key_in_headers(&headers, match_keys, path)?;
    let mut rows = Vec::new();
    for rec in rdr.records() {
        if rows.len() >= MAX_ASSET_ROWS {
            return Err(AppError::Mapping {
                path: path.into(),
                detail: format!("资产表行数超过 {MAX_ASSET_ROWS} 行上限"),
            });
        }
        let rec = rec.map_err(|e| AppError::Mapping {
            path: path.into(),
            detail: format!("解析行失败：{e}"),
        })?;
        let mut row = HashMap::new();
        for (i, val) in rec.iter().enumerate() {
            if let Some(h) = headers.get(i) {
                row.insert(h.clone(), val.to_string());
            }
        }
        inject_keys(&mut row, match_keys);
        rows.push(row);
    }
    Ok(rows)
}

fn load_excel(
    path: &str,
    match_keys: &[String],
    sheet: Option<&str>,
) -> Result<Vec<AssetRow>, AppError> {
    use calamine::{open_workbook_auto, Reader, Sheets};
    let mut book: Sheets<_> = open_workbook_auto(path).map_err(|e| AppError::Mapping {
        path: path.into(),
        detail: format!("打开 Excel 失败：{e}"),
    })?;
    let name = if let Some(s) = sheet {
        // 校验指定的工作表名是否存在于 workbook 中
        let sheet_names = book.sheet_names();
        if !sheet_names.iter().any(|sn| sn == s) {
            return Err(AppError::Mapping {
                path: path.into(),
                detail: format!(
                    "工作表「{s}」不存在（可用工作表：{}）",
                    sheet_names.join(", ")
                ),
            });
        }
        s.to_string()
    } else {
        book.sheet_names()
            .first()
            .cloned()
            .ok_or_else(|| AppError::Mapping {
                path: path.into(),
                detail: "Excel 无工作表".into(),
            })?
    };
    let range = book.worksheet_range(&name).map_err(|e| AppError::Mapping {
        path: path.into(),
        detail: format!("读取工作表「{name}」失败：{e}"),
    })?;
    let mut iter = range.rows();
    let header = iter.next().ok_or_else(|| AppError::Mapping {
        path: path.into(),
        detail: "Excel 首行（表头）为空".into(),
    })?;
    let headers: Vec<String> = header.iter().map(ToString::to_string).collect();
    validate_match_key_in_headers(&headers, match_keys, path)?;
    let mut rows = Vec::new();
    for row in iter {
        if rows.len() >= MAX_ASSET_ROWS {
            return Err(AppError::Mapping {
                path: path.into(),
                detail: format!("资产表行数超过 {MAX_ASSET_ROWS} 行上限"),
            });
        }
        let mut m = HashMap::new();
        for (i, cell) in row.iter().enumerate() {
            if let Some(h) = headers.get(i) {
                m.insert(h.clone(), cell.to_string());
            }
        }
        inject_keys(&mut m, match_keys);
        rows.push(m);
    }
    Ok(rows)
}

/// 加载 MySQL 资产表：自动执行 ``SELECT * FROM `table` ``（不手写 SQL），
/// 用文本协议读取（所有列按字符串解码，列顺序无关），并为每行注入 `@key`。
/// 查询结果必须包含全部 `match_keys` 列（列校验在 [`validate_result_columns`]）。
///
/// # Errors
///
/// 返回 [`AppError::Mapping`] 当连接失败、查询失败、列缺失或行数超限。
pub async fn load_asset_table_mysql(
    source: &MappingSource,
) -> Result<Vec<AssetRow>, AppError> {
    use sqlx::{Column, Row};
    let host = source
        .host
        .as_deref()
        .ok_or_else(|| missing_mysql_field(source, "host"))?;
    let username = source
        .username
        .as_deref()
        .ok_or_else(|| missing_mysql_field(source, "username"))?;
    let database = source
        .database
        .as_deref()
        .ok_or_else(|| missing_mysql_field(source, "database"))?;
    let table = source
        .table
        .as_deref()
        .ok_or_else(|| missing_mysql_field(source, "table"))?;
    let port = source.port.unwrap_or(3306);
    let password = source.password.as_deref().unwrap_or("");

    // IPv6 地址需用方括号包裹（RFC 3986），与 db.rs 的 URL 构建保持一致
    let host_part = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let url = format!(
        "mysql://{}:{}@{}:{}/{}",
        crate::db::percent_encode(username),
        crate::db::percent_encode(password),
        host_part,
        port,
        crate::db::percent_encode(database)
    );
    let path_label = format!("mysql://{host}:{port}/{database}/{table}");

    let pool = sqlx::pool::PoolOptions::<sqlx::MySql>::new()
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(&url)
        .await
        .map_err(|e| AppError::Mapping {
            path: path_label.clone(),
            detail: format!("无法连接 MySQL {host}:{port} 数据库「{database}」：{e}"),
        })?;

    let result = async {
        // 文本协议查询：所有列按字符串返回，避免按类型解码的麻烦
        let sql = format!("SELECT * FROM `{}`", escape_identifier(table));
        let rows = sqlx::raw_sql(&sql).fetch_all(&pool).await.map_err(|e| {
            AppError::Mapping {
                path: path_label.clone(),
                detail: format!("查询资产表「{table}」失败：{e}"),
            }
        })?;

        // 空结果集没有列元数据，直接返回空资产（与空 CSV 行为一致）
        if rows.is_empty() {
            return Ok::<Vec<AssetRow>, AppError>(Vec::new());
        }
        let columns: Vec<String> = rows[0]
            .columns()
            .iter()
            .map(|c| c.name().to_string())
            .collect();
        validate_result_columns(&columns, source, &path_label)?;

        let mut assets = Vec::new();
        for row in rows {
            if assets.len() >= MAX_ASSET_ROWS {
                return Err(AppError::Mapping {
                    path: path_label.clone(),
                    detail: format!("资产表行数超过 {MAX_ASSET_ROWS} 行上限"),
                });
            }
            let mut m = HashMap::new();
            for (i, col) in row.columns().iter().enumerate() {
                let val = row
                    .try_get::<Option<String>, _>(i)
                    .unwrap_or_default()
                    .unwrap_or_default();
                m.insert(col.name().to_string(), val);
            }
            inject_keys(&mut m, &source.match_keys);
            assets.push(m);
        }
        Ok(assets)
    }
    .await;

    pool.close().await;
    result
}

fn missing_mysql_field(source: &MappingSource, field: &str) -> AppError {
    AppError::Mapping {
        path: source.source_path.clone(),
        detail: format!(
            "source_type: mysql 的映射来源缺少必填字段「{field}」（需配置 host/username/database/table）"
        ),
    }
}

/// 校验 MySQL 查询结果的列：必须包含全部 `match_keys` 列与全部
/// `source_field` 注入列；缺失时返回错误。
fn validate_result_columns(
    columns: &[String],
    source: &MappingSource,
    path: &str,
) -> Result<(), AppError> {
    let mut missing: Vec<&str> = Vec::new();
    for key in &source.match_keys {
        if !columns.iter().any(|c| c == key) {
            missing.push(key);
        }
    }
    for col in &source.columns {
        if !columns.iter().any(|c| c == &col.source_field) {
            missing.push(&col.source_field);
        }
    }
    if !missing.is_empty() {
        return Err(AppError::Mapping {
            path: path.into(),
            detail: format!(
                "资产表查询结果缺少列：{}（实际可用列：{}）",
                missing.join("、"),
                columns.join(", ")
            ),
        });
    }
    Ok(())
}

/// 转义 MySQL 反引号标识符（反引号翻倍），防止表名注入。
fn escape_identifier(ident: &str) -> String {
    ident.replace('`', "``")
}

/// 正则模式的单键索引（`asset_pattern` 方向）：资产侧列值是模式。
///
/// 不含通配符元字符的模式按字面量建哈希索引（O(1) 查找，IP 等常规值
/// 不需要正则扫描）；含元字符的模式预编译为锚定正则后逐个匹配。
#[derive(Debug)]
struct KeyPatternIndex {
    /// 字面量模式 → 命中的行下标列表。
    literal_rows: HashMap<String, Vec<usize>>,
    /// (行下标, 锚定正则) 列表，按资产行顺序。
    regex_rows: Vec<(usize, Regex)>,
}

/// 正则模式元字符集合（`*+?()[]{}^$|\`）。
/// 注意：点号 `.` **不**在集合内——资产表里的 IP（如 "10.0.1.5"）若含点号
/// 就被当作字面量精确匹配，避免 1M 行 IP 全部退化为正则扫描；若确需点号
/// 通配语义，请配合其它元字符使用（如 `10.0.1.5.*` 含 `*` 会走正则）。
fn has_regex_metachars(s: &str) -> bool {
    s.chars().any(|c| matches!(c, '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '^' | '$' | '|' | '\\'))
}

/// 构建锚定正则：自动包裹 `^...$` 实现全值匹配（用户自带锚点也无害——
/// `^`/`$` 是零宽断言，连续出现合法）。
fn anchor_pattern(pattern: &str) -> String {
    format!("^{pattern}$")
}

/// 资产匹配索引（按来源构建，三种匹配形态之一）。
#[derive(Debug)]
pub struct AssetIndex {
    /// 精确模式：组合键（多键以分隔符拼接）→ 行下标。
    exact: Option<HashMap<String, usize>>,
    /// 正则 + asset_pattern 方向：每键的模式索引。
    asset_pattern_keys: Option<Vec<KeyPatternIndex>>,
    /// 正则 + record_pattern 方向：每键的资产字面量值 → 行下标。
    record_pattern_keys: Option<Vec<HashMap<String, Vec<usize>>>>,
    /// 资产行（下标即索引键）。
    rows: Vec<AssetRow>,
    /// 资产侧匹配列名（`match_keys`）。
    keys: Vec<String>,
    /// CardRecord 侧字段名（`record_key` 解析结果，与 keys 一一对应）。
    record_fields: Vec<String>,
    /// 注入列映射（source_field → rename）。
    columns: Vec<MappingColumn>,
    mode: MatchMode,
    direction: MatchDirection,
}

/// 单条记录 join 的结果。
#[derive(Debug, Default)]
pub struct JoinResult {
    /// 注入的 (rename → value) 映射；未命中时为空。
    pub values: HashMap<String, String>,
    /// 匹配过程中产生的警告（如正则模式多行命中取首行、记录侧非法正则）。
    pub warnings: Vec<String>,
}

/// 从资产行列表构建匹配索引（按来源的 match_mode / match_direction）。
///
/// - 精确模式：同一组合键出现多次时取首行，并返回重复 key 警告。
/// - 正则 + asset_pattern：资产侧模式非法（编译失败）时返回错误（该来源失败，
///   由调用方记 Warning 继续）。
///
/// # Errors
///
/// 返回 [`AppError::Mapping`] 当 match_mode/match_direction 非法、
/// record_key 与 match_keys 数量不一致、或资产侧正则模式无法编译。
pub fn build_asset_index(
    assets: &[AssetRow],
    source: &MappingSource,
) -> Result<(AssetIndex, Vec<String>), AppError> {
    let mode = source.parse_match_mode()?;
    let direction = source.parse_match_direction()?;
    let record_fields = source.record_field_names()?;
    let keys = source.match_keys.clone();

    let (exact, asset_pattern_keys, record_pattern_keys, warnings) = match (mode, direction) {
        (MatchMode::Exact, _) => {
            let mut map = HashMap::with_capacity(assets.len());
            let mut dup_warnings = Vec::new();
            for (i, row) in assets.iter().enumerate() {
                if let Some(key) = row.get("@key") {
                    if map.contains_key(key) {
                        dup_warnings.push(format!(
                            "资产表 @key「{key}」重复，仅保留首行（跳过后续重复行）"
                        ));
                    } else {
                        map.insert(key.clone(), i);
                    }
                }
            }
            (Some(map), None, None, dup_warnings)
        }
        (MatchMode::Regex, MatchDirection::AssetPattern) => {
            // 每键一个模式索引：字面量进哈希，元字符模式编译锚定正则
            let mut per_key: Vec<KeyPatternIndex> = Vec::new();
            for key in &keys {
                let mut literal_rows: HashMap<String, Vec<usize>> = HashMap::new();
                let mut regex_rows: Vec<(usize, Regex)> = Vec::new();
                for (i, row) in assets.iter().enumerate() {
                    let pattern = row.get(key).cloned().unwrap_or_default();
                    if pattern.is_empty() {
                        // 空模式按字面量空串处理（只匹配空记录值）
                        literal_rows.entry(pattern).or_default().push(i);
                    } else if has_regex_metachars(&pattern) {
                        let re = Regex::new(&anchor_pattern(&pattern)).map_err(|e| {
                            AppError::Mapping {
                                path: source.source_path.clone(),
                                detail: format!(
                                    "资产表「{key}」列的模式「{pattern}」不是合法正则：{e}"
                                ),
                            }
                        })?;
                        regex_rows.push((i, re));
                    } else {
                        literal_rows.entry(pattern).or_default().push(i);
                    }
                }
                per_key.push(KeyPatternIndex {
                    literal_rows,
                    regex_rows,
                });
            }
            (None, Some(per_key), None, Vec::new())
        }
        (MatchMode::Regex, MatchDirection::RecordPattern) => {
            // 资产侧值一律是字面量：按值建哈希索引（含重复值 → 多个行下标）
            let mut per_key: Vec<HashMap<String, Vec<usize>>> = Vec::new();
            for key in &keys {
                let mut by_value: HashMap<String, Vec<usize>> = HashMap::new();
                for (i, row) in assets.iter().enumerate() {
                    let val = row.get(key).cloned().unwrap_or_default();
                    by_value.entry(val).or_default().push(i);
                }
                per_key.push(by_value);
            }
            (None, None, Some(per_key), Vec::new())
        }
    };

    Ok((
        AssetIndex {
            exact,
            asset_pattern_keys,
            record_pattern_keys,
            rows: assets.to_vec(),
            keys,
            record_fields,
            columns: source.columns.clone(),
            mode,
            direction,
        },
        warnings,
    ))
}

impl AssetIndex {
    /// 匹配一条记录的所有键，返回命中行下标列表（按资产行顺序）。
    /// 匹配过程中的非致命问题（非法记录正则等）记入 warnings。
    fn matching_rows(&self, rec: &CardRecord) -> (Vec<usize>, Vec<String>) {
        let mut warnings = Vec::new();
        match self.mode {
            MatchMode::Exact => {
                let key = build_record_key(rec, &self.record_fields);
                let hit = self
                    .exact
                    .as_ref()
                    .and_then(|m| m.get(&key).copied())
                    .map_or_else(Vec::new, |i| vec![i]);
                (hit, warnings)
            }
            MatchMode::Regex => match self.direction {
                MatchDirection::AssetPattern => {
                    // 每键计算命中行集合，逐键取交集（AND 组合）
                    let mut cand: Vec<bool> = vec![true; self.rows.len()];
                    let patterns = self
                        .asset_pattern_keys
                        .as_ref()
                        .expect("asset_pattern mode must have pattern index");
                    for (k, kp) in patterns.iter().enumerate() {
                        let v = card_record_field(rec, &self.record_fields[k]);
                        let mut hit: HashSet<usize> = HashSet::new();
                        if let Some(list) = kp.literal_rows.get(&v) {
                            hit.extend(list.iter().copied());
                        }
                        for (ri, re) in &kp.regex_rows {
                            if re.is_match(&v) {
                                hit.insert(*ri);
                            }
                        }
                        if hit.is_empty() {
                            return (Vec::new(), warnings);
                        }
                        for (i, c) in cand.iter_mut().enumerate() {
                            *c &= hit.contains(&i);
                        }
                    }
                    (
                        cand.iter()
                            .enumerate()
                            .filter(|(_, c)| **c)
                            .map(|(i, _)| i)
                            .collect(),
                        warnings,
                    )
                }
                MatchDirection::RecordPattern => {
                    // 记录侧值是模式：逐键匹配资产侧字面量，逐键取交集
                    let mut cand: Vec<bool> = vec![true; self.rows.len()];
                    let by_value = self
                        .record_pattern_keys
                        .as_ref()
                        .expect("record_pattern mode must have literal index");
                    for (k, key_col) in self.keys.iter().enumerate() {
                        let v = card_record_field(rec, &self.record_fields[k]);
                        let mut hit: HashSet<usize> = HashSet::new();
                        if !has_regex_metachars(&v) {
                            // 记录值无元字符：按字面量精确查找（O(1)）
                            if let Some(list) = by_value[k].get(&v) {
                                hit.extend(list.iter().copied());
                            }
                        } else {
                            // 记录值含元字符：编译锚定正则后扫描资产值
                            match Regex::new(&anchor_pattern(&v)) {
                                Ok(re) => {
                                    for (i, row) in self.rows.iter().enumerate() {
                                        if let Some(val) = row.get(key_col) {
                                            if re.is_match(val) {
                                                hit.insert(i);
                                            }
                                        }
                                    }
                                }
                                Err(_) => {
                                    warnings.push(format!(
                                        "记录「{}」的匹配值「{v}」不是合法正则，该键视为不匹配",
                                        rec.host_ip
                                    ));
                                }
                            }
                        }
                        if hit.is_empty() {
                            return (Vec::new(), warnings);
                        }
                        for (i, c) in cand.iter_mut().enumerate() {
                            *c &= hit.contains(&i);
                        }
                    }
                    (
                        cand.iter()
                            .enumerate()
                            .filter(|(_, c)| **c)
                            .map(|(i, _)| i)
                            .collect(),
                        warnings,
                    )
                }
            },
        }
    }
}

/// 对一行 `CardRecord` 做单来源 join，返回注入值 + 匹配警告。
/// 未命中时 values 为空（调用方据此统计命中率）。
#[must_use]
pub fn join_record(rec: &CardRecord, index: &AssetIndex) -> JoinResult {
    let (rows, mut warnings) = index.matching_rows(rec);
    let mut out = HashMap::new();
    if let Some(&first) = rows.first() {
        if rows.len() > 1 {
            warnings.push(format!(
                "记录「{}/{}」命中 {} 行资产，仅取首行（其余行被忽略）",
                rec.host_ip,
                rec.card_id,
                rows.len()
            ));
        }
        let row = &index.rows[first];
        for col in &index.columns {
            if let Some(v) = row.get(&col.source_field) {
                out.insert(col.rename.clone(), v.clone());
            }
        }
    }
    JoinResult { values: out, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono::Utc;

    #[test]
    fn paired_arrays_have_matching_lengths() {
        assert_eq!(CORE_BASE_COLUMNS.len(), CORE_BASE_LOCAL_NAMES.len(),
            "CORE_BASE_COLUMNS 与 CORE_BASE_LOCAL_NAMES 长度必须一致");
        assert_eq!(TEMP_COLUMNS.len(), TEMP_LOCAL_NAMES.len(),
            "TEMP_COLUMNS 与 TEMP_LOCAL_NAMES 长度必须一致");
        assert_eq!(POWER_COLUMNS.len(), POWER_LOCAL_NAMES.len(),
            "POWER_COLUMNS 与 POWER_LOCAL_NAMES 长度必须一致");
        assert_eq!(HOST_CPU_COLUMNS.len(), HOST_CPU_LOCAL_NAMES.len(),
            "HOST_CPU_COLUMNS 与 HOST_CPU_LOCAL_NAMES 长度必须一致");
        assert_eq!(HOST_MEM_COLUMNS.len(), HOST_MEM_LOCAL_NAMES.len(),
            "HOST_MEM_COLUMNS 与 HOST_MEM_LOCAL_NAMES 长度必须一致");
        assert_eq!(HOST_HANDLE_COLUMNS.len(), HOST_HANDLE_LOCAL_NAMES.len(),
            "HOST_HANDLE_COLUMNS 与 HOST_HANDLE_LOCAL_NAMES 长度必须一致");
    }

    #[test]
    fn build_base_columns_and_local_names_same_length() {
        let flags = ColumnFlags { has_temp: true, has_power: true, has_host_cpu: true, has_host_mem: true, has_host_handle: true };
        let cols = build_base_columns(flags);
        let names = build_base_local_names(flags);
        assert_eq!(cols.len(), names.len(), "build_base_columns 与 build_base_local_names 长度必须一致");
    }

    #[test]
    fn paired_arrays_are_positionally_aligned() {
        // 校验每对并行数组在 build_base_pairs 中 zip 后产出一致的 (display, local) 对。
        // 防止元素位置错位（长度相同但内容对不上）导致数据库列映射写错字段。
        let check = |display: &[&str], local: &[&str], label: &str| {
            for (i, (d, l)) in display.iter().zip(local.iter()).enumerate() {
                // 校验 local_name 是合法的 snake_case 标识符（纯小写+下划线+数字），
                // display_name 不应匹配此模式（它是中文显示名）——若位置错位，
                // 会出现中文跑到 local_name 或英文跑到 display_name 的情况。
                assert!(
                    l.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()),
                    "{label}[{i}]：local_name「{l}」不像合法的字段标识符，可能位置与 display_name「{d}」错位"
                );
            }
        };
        check(CORE_BASE_COLUMNS, CORE_BASE_LOCAL_NAMES, "CORE_BASE");
        check(TEMP_COLUMNS, TEMP_LOCAL_NAMES, "TEMP");
        check(POWER_COLUMNS, POWER_LOCAL_NAMES, "POWER");
        check(HOST_CPU_COLUMNS, HOST_CPU_LOCAL_NAMES, "HOST_CPU");
        check(HOST_MEM_COLUMNS, HOST_MEM_LOCAL_NAMES, "HOST_MEM");
        check(HOST_HANDLE_COLUMNS, HOST_HANDLE_LOCAL_NAMES, "HOST_HANDLE");
    }

    fn rec(ip: &str, card: &str) -> CardRecord {
        CardRecord {
            source_name: "s".into(),
            host_ip: ip.into(),
            node_name: String::new(),
            card_id: card.into(),
            device_type: "X".into(),
            namespace: String::new(),
            pod: String::new(),
            container: String::new(),
            core_avg: None,
            core_peak: None,
            core_peak_time: None,
            core_count: None,
            core_first_time: None,
            core_last_time: None,
            mem_avg: None,
            mem_peak: None,
            mem_peak_time: None,
            mem_count: None,
            mem_first_time: None,
            mem_last_time: None,
            temp_avg: None,
            temp_peak: None,
            temp_peak_time: None,
            temp_count: None,
            temp_first_time: None,
            temp_last_time: None,
            power_avg: None,
            power_peak: None,
            power_peak_time: None,
            power_count: None,
            power_first_time: None,
            power_last_time: None,
            host_cpu_avg: None,
            host_cpu_peak: None,
            host_cpu_peak_time: None,
            host_mem_avg: None,
            host_mem_peak: None,
            host_mem_peak_time: None,
            host_handle_avg: None,
            host_handle_peak: None,
            host_handle_peak_time: None,
            range_start: Utc.timestamp_opt(0, 0).unwrap(),
            range_end: Utc.timestamp_opt(60, 0).unwrap(),
        }
    }

    #[test]
    fn column_order_inserts_after_anchor() {
        // 两个映射列都锚定到同一基础列"主机IP"（PRD §2.3 锚点约束：锚点必须为基础列）。
        // 同 index 的多列按配置顺序堆叠：机房在前、负责人在后。
        let cols = vec![
            MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            },
            MappingColumn {
                source_field: "负责人".into(),
                rename: "负责人".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            },
        ];
        let order = compute_column_order(BASE_COLUMNS, &cols);
        let ip = order.iter().position(|s| s == "主机IP").unwrap();
        let room = order.iter().position(|s| s == "机房").unwrap();
        let owner = order.iter().position(|s| s == "负责人").unwrap();
        assert_eq!(room, ip + 1);
        assert_eq!(owner, ip + 2);
    }

    #[test]
    fn column_order_before_anchor() {
        let cols = vec![MappingColumn {
            source_field: "x".into(),
            rename: "X".into(),
            local_name: None,
            position: InsertPosition::before("设备类型"),
        }];
        let order = compute_column_order(BASE_COLUMNS, &cols);
        let x = order.iter().position(|s| s == "X").unwrap();
        let dt = order.iter().position(|s| s == "设备类型").unwrap();
        assert_eq!(x + 1, dt);
    }

    #[test]
    fn column_order_missing_anchor_appends() {
        let cols = vec![MappingColumn {
            source_field: "x".into(),
            rename: "X".into(),
            local_name: None,
            position: InsertPosition::after("不存在"),
        }];
        let order = compute_column_order(BASE_COLUMNS, &cols);
        assert_eq!(order.last().unwrap(), "X");
    }

    #[test]
    fn column_order_missing_anchor_after_valid_after_last_base() {
        // 缺失锚点列应排在末尾，不与指向末尾基础列 After 的有效锚点列交错。
        // BASE_COLUMNS 最后一列是"显存占用率末条数据时间"。
        let last_base = BASE_COLUMNS.last().unwrap();
        let cols = vec![
            MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("不存在"), // 缺失锚点，应追加到末尾
            },
            MappingColumn {
                source_field: "负责人".into(),
                rename: "负责人".into(),
                local_name: None,
                position: InsertPosition::after(*last_base), // 有效锚点，最后一列之后
            },
        ];
        let order = compute_column_order(BASE_COLUMNS, &cols);
        let last_base_idx = order.iter().position(|s| s == *last_base).unwrap();
        let owner_idx = order.iter().position(|s| s == "负责人").unwrap();
        let room_idx = order.iter().position(|s| s == "机房").unwrap();
        // 有效锚点列紧随锚点之后
        assert_eq!(owner_idx, last_base_idx + 1);
        // 缺失锚点列在有效锚点列之后（追加到末尾）
        assert!(room_idx > owner_idx, "缺失锚点列「机房」应在有效锚点列「负责人」之后，实际 order: {order:?}");
    }

    /// 构造测试用映射来源（默认 file/exact/asset_pattern）。
    fn test_source(match_keys: &[&str], columns: Vec<MappingColumn>) -> MappingSource {
        MappingSource {
            source_path: String::new(),
            match_keys: match_keys.iter().map(|k| (*k).to_string()).collect(),
            columns,
            ..MappingSource::default()
        }
    }

    /// 构建资产行（按 source 的 match_keys 注入 @key）。
    fn asset_row(pairs: &[(&str, &str)], source: &MappingSource) -> AssetRow {
        let mut row = HashMap::new();
        for (k, v) in pairs {
            row.insert((*k).to_string(), (*v).to_string());
        }
        inject_keys(&mut row, &source.match_keys);
        row
    }

    /// 构建索引并断言成功，返回 (index, warnings)。
    fn build_index(
        assets: &[AssetRow],
        source: &MappingSource,
    ) -> (AssetIndex, Vec<String>) {
        build_asset_index(assets, source).expect("索引构建应成功")
    }

    #[test]
    fn join_record_hits_and_misses() {
        let source = test_source(
            &["host_ip"],
            vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
        );
        let a1 = asset_row(&[("host_ip", "1.1.1.1"), ("机房", "北京A")], &source);
        let (index, _) = build_index(&[a1], &source);

        let hit = join_record(&rec("1.1.1.1", "0"), &index);
        assert_eq!(hit.values.get("机房").unwrap(), "北京A");

        let miss = join_record(&rec("2.2.2.2", "0"), &index);
        assert!(miss.values.is_empty());
    }

    #[test]
    fn join_record_with_custom_key_name() {
        // 资产表用 "IP地址" 作为匹配列，CardRecord 用 "host_ip"
        let source = MappingSource {
            source_path: String::new(),
            match_keys: vec!["IP地址".into()],
            record_key: vec!["host_ip".into()],
            columns: vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
            ..MappingSource::default()
        };
        let a1 = asset_row(&[("IP地址", "1.1.1.1"), ("机房", "北京A")], &source);
        let (index, _) = build_index(&[a1], &source);

        // CardRecord 的 host_ip 字段值 "1.1.1.1" 通过 record_key 映射，
        // 应能匹配到资产表的 "IP地址" 列值
        let hit = join_record(&rec("1.1.1.1", "0"), &index);
        assert_eq!(hit.values.get("机房").unwrap(), "北京A");
    }

    #[test]
    fn join_record_with_unknown_key_returns_empty() {
        // match_keys 指定了 CardRecord 不存在的字段名 → join key 为空串 → 不会命中
        let source = test_source(
            &["unknown_column"],
            vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
        );
        let a1 = asset_row(&[("unknown_column", "1.1.1.1"), ("机房", "北京A")], &source);
        let (index, _) = build_index(&[a1], &source);

        let miss = join_record(&rec("1.1.1.1", "0"), &index);
        assert!(
            miss.values.is_empty(),
            "未知字段名应导致 join key 为空串，不会命中"
        );
    }

    #[test]
    fn join_record_multi_key_and() {
        // 多键 AND：host_ip 与 card_id 都命中才注入
        let source = test_source(
            &["host_ip", "card_id"],
            vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
        );
        let a1 = asset_row(&[("host_ip", "1.1.1.1"), ("card_id", "GPU-0"), ("机房", "北京A")], &source);
        let (index, _) = build_index(&[a1], &source);

        // 两个键都命中
        let r = rec("1.1.1.1", "GPU-0");
        let hit = join_record(&r, &index);
        assert_eq!(hit.values.get("机房").unwrap(), "北京A");

        // 仅一个键命中 → 不注入（AND 组合）
        let r = rec("1.1.1.1", "GPU-1");
        assert!(join_record(&r, &index).values.is_empty());

        let r = rec("2.2.2.2", "GPU-0");
        assert!(join_record(&r, &index).values.is_empty());
    }

    #[test]
    fn join_record_multi_key_with_custom_record_keys() {
        // 资产表列名 [IP地址, 卡号] → CardRecord 字段 [host_ip, card_id]
        let source = MappingSource {
            source_path: String::new(),
            match_keys: vec!["IP地址".into(), "卡号".into()],
            record_key: vec!["host_ip".into(), "card_id".into()],
            columns: vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
            ..MappingSource::default()
        };
        let a1 = asset_row(&[("IP地址", "1.1.1.1"), ("卡号", "0"), ("机房", "北京A")], &source);
        let (index, _) = build_index(&[a1], &source);

        let hit = join_record(&rec("1.1.1.1", "0"), &index);
        assert_eq!(hit.values.get("机房").unwrap(), "北京A");

        let r = rec("1.1.1.1", "1");
        assert!(join_record(&r, &index).values.is_empty());
    }

    #[test]
    fn join_record_regex_asset_pattern_wildcard() {
        // asset_pattern：资产侧值是模式（如 "10.0.1.*"），匹配记录侧 IP
        let source = MappingSource {
            source_path: String::new(),
            match_keys: vec!["host_ip".into()],
            match_mode: "regex".into(),
            match_direction: "asset_pattern".into(),
            columns: vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
            ..MappingSource::default()
        };
        let a1 = asset_row(&[("host_ip", "10.0.1.*"), ("机房", "北京A")], &source);
        let a2 = asset_row(&[("host_ip", "192.168.*.*"), ("机房", "北京B")], &source);
        let (index, _) = build_index(&[a1, a2], &source);

        let hit = join_record(&rec("10.0.1.5", "0"), &index);
        assert_eq!(hit.values.get("机房").unwrap(), "北京A");
        let hit = join_record(&rec("10.0.1.55", "0"), &index);
        assert_eq!(hit.values.get("机房").unwrap(), "北京A");
        let hit = join_record(&rec("192.168.3.9", "0"), &index);
        assert_eq!(hit.values.get("机房").unwrap(), "北京B");
        // 无匹配
        let miss = join_record(&rec("172.16.0.1", "0"), &index);
        assert!(miss.values.is_empty());
    }

    #[test]
    fn join_record_regex_anchored_full_match() {
        // 全值锚定：模式 "10.0.1.5"（点号按字面量）不应命中 "10.0.1.55"，
        // 也不应命中前缀/后缀的更长 IP（^...$ 包裹）
        let source = MappingSource {
            source_path: String::new(),
            match_keys: vec!["host_ip".into()],
            match_mode: "regex".into(),
            match_direction: "asset_pattern".into(),
            columns: vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
            ..MappingSource::default()
        };
        let a1 = asset_row(&[("host_ip", "10.0.1.5"), ("机房", "北京A")], &source);
        let (index, _) = build_index(&[a1], &source);

        assert_eq!(
            join_record(&rec("10.0.1.5", "0"), &index)
                .values
                .get("机房")
                .unwrap(),
            "北京A"
        );
        // 字面量模式："10.0.1.55" 与 "110.0.1.5" 都不应命中（点号按字面量 + 锚定）
        assert!(join_record(&rec("10.0.1.55", "0"), &index).values.is_empty());
        assert!(join_record(&rec("110.0.1.5", "0"), &index).values.is_empty());
    }

    #[test]
    fn join_record_regex_multi_key_and() {
        // 正则多键 AND：IP 模式与卡号模式都命中才注入
        let source = MappingSource {
            source_path: String::new(),
            match_keys: vec!["host_ip".into(), "card_id".into()],
            match_mode: "regex".into(),
            match_direction: "asset_pattern".into(),
            columns: vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
            ..MappingSource::default()
        };
        let a1 = asset_row(&[("host_ip", "10.0.1.*"), ("card_id", "GPU-[0-9]+"), ("机房", "北京A")], &source);
        let (index, _) = build_index(&[a1], &source);

        // 双键都命中
        let hit = join_record(&rec("10.0.1.5", "GPU-7"), &index);
        assert_eq!(hit.values.get("机房").unwrap(), "北京A");
        // 卡号不匹配 → 不注入
        let r = rec("10.0.1.5", "GPU-X");
        assert!(join_record(&r, &index).values.is_empty());
        // IP 不匹配 → 不注入
        let r = rec("172.16.0.1", "GPU-7");
        assert!(join_record(&r, &index).values.is_empty());
    }

    #[test]
    fn join_record_regex_record_pattern() {
        // record_pattern：记录侧值是模式（如 "GPU-\\d+"），匹配资产侧字面量
        let source = MappingSource {
            source_path: String::new(),
            match_keys: vec!["card_id".into()],
            match_mode: "regex".into(),
            match_direction: "record_pattern".into(),
            columns: vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
            ..MappingSource::default()
        };
        let a1 = asset_row(&[("card_id", "GPU-0"), ("机房", "北京A")], &source);
        let a2 = asset_row(&[("card_id", "GPU-7"), ("机房", "北京B")], &source);
        let (index, _) = build_index(&[a1, a2], &source);

        // 记录值为模式：命中资产侧字面量
        let r = rec("1.1.1.1", "GPU-[0-9]+");
        let hit = join_record(&r, &index);
        assert_eq!(hit.values.get("机房").unwrap(), "北京A", "首行命中优先");
        // 记录值为字面量（无元字符）→ 精确匹配
        let r = rec("1.1.1.1", "GPU-7");
        let hit = join_record(&r, &index);
        assert_eq!(hit.values.get("机房").unwrap(), "北京B");
    }

    #[test]
    fn join_record_regex_invalid_record_pattern_warns_and_misses() {
        // 记录侧非法正则：该键视为不匹配 + Warning，不崩溃
        let source = MappingSource {
            source_path: String::new(),
            match_keys: vec!["card_id".into()],
            match_mode: "regex".into(),
            match_direction: "record_pattern".into(),
            columns: vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
            ..MappingSource::default()
        };
        let a1 = asset_row(&[("card_id", "GPU-0"), ("机房", "北京A")], &source);
        let (index, _) = build_index(&[a1], &source);

        // "GPU-[" 含未闭合的 [ → 非法正则
        let r = rec("1.1.1.1", "GPU-[");
        let result = join_record(&r, &index);
        assert!(result.values.is_empty(), "非法记录正则不应命中");
        assert!(
            result.warnings.iter().any(|w| w.contains("不是合法正则")),
            "应产生非法正则警告"
        );
    }

    #[test]
    fn build_asset_index_rejects_invalid_asset_pattern() {
        // asset_pattern 方向：资产侧模式非法 → 索引构建失败（该来源失败）
        let source = MappingSource {
            source_path: "assets.csv".into(),
            match_keys: vec!["host_ip".into()],
            match_mode: "regex".into(),
            match_direction: "asset_pattern".into(),
            columns: vec![],
            ..MappingSource::default()
        };
        let a1 = asset_row(&[("host_ip", "10.0.[*"), ("机房", "北京A")], &source);
        let err = build_asset_index(&[a1], &source).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("不是合法正则"), "应指出非法模式：{msg}");
    }

    #[test]
    fn join_record_regex_multi_hit_takes_first_with_warning() {
        // 一条记录命中多行资产：取首行 + Warning
        let source = MappingSource {
            source_path: String::new(),
            match_keys: vec!["host_ip".into()],
            match_mode: "regex".into(),
            match_direction: "asset_pattern".into(),
            columns: vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
            ..MappingSource::default()
        };
        // "10.0.1.*" 与 "10.0.*.*" 都能匹配 10.0.1.5
        let a1 = asset_row(&[("host_ip", "10.0.1.*"), ("机房", "北京A")], &source);
        let a2 = asset_row(&[("host_ip", "10.0.*.*"), ("机房", "北京B")], &source);
        let (index, _) = build_index(&[a1, a2], &source);

        let result = join_record(&rec("10.0.1.5", "0"), &index);
        assert_eq!(result.values.get("机房").unwrap(), "北京A", "取首行");
        assert!(
            result.warnings.iter().any(|w| w.contains("命中 2 行资产")),
            "应产生多行命中警告"
        );
    }

    #[test]
    fn build_asset_index_rejects_record_key_length_mismatch() {
        let source = MappingSource {
            source_path: String::new(),
            match_keys: vec!["host_ip".into(), "card_id".into()],
            record_key: vec!["host_ip".into()], // 数量不一致
            columns: vec![],
            ..MappingSource::default()
        };
        let err = build_asset_index(&[], &source).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("数量不一致"), "应指出数量不一致：{msg}");
    }

    #[test]
    fn parse_match_mode_and_direction_validation() {
        let mut source = test_source(&["host_ip"], vec![]);
        assert_eq!(source.parse_match_mode().unwrap(), MatchMode::Exact);
        source.match_mode = "regex".into();
        assert_eq!(source.parse_match_mode().unwrap(), MatchMode::Regex);
        source.match_mode = "fuzzy".into();
        let err = format!("{}", source.parse_match_mode().unwrap_err());
        assert!(err.contains("fuzzy"), "应指出非法 match_mode：{err}");

        source.match_mode = "regex".into();
        source.match_direction = "both".into();
        let err = format!("{}", source.parse_match_direction().unwrap_err());
        assert!(err.contains("both"), "应指出非法 match_direction：{err}");
    }

    #[test]
    fn match_keys_accepts_string_or_list_in_yaml() {
        // 旧配置：单字符串；新配置：列表——都必须能反序列化
        let single: MappingSource = serde_yaml_ng::from_str(
            "source_path: a.csv\nmatch_keys: host_ip\ncolumns: []",
        )
        .unwrap();
        assert_eq!(single.match_keys, vec!["host_ip".to_string()]);

        let list: MappingSource = serde_yaml_ng::from_str(
            "source_path: a.csv\nmatch_keys: [host_ip, card_id]\ncolumns: []",
        )
        .unwrap();
        assert_eq!(list.match_keys, vec!["host_ip".to_string(), "card_id".to_string()]);

        // 旧配置 record_key 单字符串兼容
        let old: MappingSource = serde_yaml_ng::from_str(
            "source_path: a.csv\nmatch_keys: IP地址\nrecord_key: host_ip\ncolumns: []",
        )
        .unwrap();
        assert_eq!(old.record_key, vec!["host_ip".to_string()]);
        // 新字段默认值
        assert_eq!(old.source_type, "file");
        assert_eq!(old.match_mode, "exact");
        assert_eq!(old.match_direction, "asset_pattern");
    }

    #[test]
    fn multi_source_mapping() {
        // 两个来源：机房表用 host_ip 匹配，负责人表用 node_name 匹配
        let src_room = test_source(
            &["host_ip"],
            vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
        );
        let src_owner = test_source(
            &["node_name"],
            vec![MappingColumn {
                source_field: "负责人".into(),
                rename: "负责人".into(),
                local_name: None,
                position: InsertPosition::after("机房"),
            }],
        );

        // 机房表
        let a1 = asset_row(&[("host_ip", "1.1.1.1"), ("机房", "北京A")], &src_room);
        let (room_index, _) = build_index(&[a1], &src_room);

        // 负责人表
        let a2 = asset_row(&[("node_name", "node-1"), ("负责人", "张三")], &src_owner);
        let (owner_index, _) = build_index(&[a2], &src_owner);

        let mut r = rec("1.1.1.1", "0");
        r.node_name = "node-1".into();
        let room_vals = join_record(&r, &room_index);
        assert_eq!(room_vals.values.get("机房").unwrap(), "北京A");
        let owner_vals = join_record(&r, &owner_index);
        assert_eq!(owner_vals.values.get("负责人").unwrap(), "张三");
    }

    #[test]
    fn multi_source_with_custom_record_key() {
        // 资产表用 "主机名" 列名，CardRecord 用 node_name 字段
        let src_owner = MappingSource {
            source_path: String::new(),
            match_keys: vec!["主机名".into()],
            record_key: vec!["node_name".into()],
            columns: vec![MappingColumn {
                source_field: "负责人".into(),
                rename: "负责人".into(),
                local_name: None,
                position: InsertPosition::after("机房"),
            }],
            ..MappingSource::default()
        };

        let a2 = asset_row(&[("主机名", "node-1"), ("负责人", "张三")], &src_owner);
        let (owner_index, _) = build_index(&[a2], &src_owner);

        let mut r = rec("1.1.1.1", "0");
        r.node_name = "node-1".into();
        let owner_vals = join_record(&r, &owner_index);
        assert_eq!(owner_vals.values.get("负责人").unwrap(), "张三");
    }

    #[test]
    fn missing_anchor_warnings_reports_non_base_anchors() {
        // PRD §2.3：锚点必须是基础列。一个合法 + 一个非法锚点。
        let cols = vec![
            MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"), // 合法（基础列）
            },
            MappingColumn {
                source_field: "x".into(),
                rename: "X".into(),
                local_name: None,
                position: InsertPosition::after("不存在"), // 非法
            },
        ];
        let ws = missing_anchor_warnings(BASE_COLUMNS, &cols);
        assert_eq!(ws.len(), 1, "只对非法锚点产出 Warning");
        assert!(ws[0].contains('X'));
        assert!(ws[0].contains("不存在"));
    }

    #[test]
    fn missing_anchor_warnings_empty_for_all_base_anchors() {
        let cols = vec![MappingColumn {
            source_field: "机房".into(),
            rename: "机房".into(),
            local_name: None,
            position: InsertPosition::before("设备类型"),
        }];
        assert!(missing_anchor_warnings(BASE_COLUMNS, &cols).is_empty());
    }

    #[test]
    fn card_record_field_known_keys() {
        let r = rec("10.0.0.1", "3");
        assert_eq!(card_record_field(&r, "source_name"), "s");
        assert_eq!(card_record_field(&r, "host_ip"), "10.0.0.1");
        assert_eq!(card_record_field(&r, "card_id"), "3");
        assert_eq!(card_record_field(&r, "device_type"), "X");
        assert_eq!(card_record_field(&r, "node_name"), "");
        assert_eq!(card_record_field(&r, "namespace"), "");
        assert_eq!(card_record_field(&r, "pod"), "");
        assert_eq!(card_record_field(&r, "container"), "");
    }

    #[test]
    fn card_record_field_unknown_key_returns_empty() {
        let r = rec("10.0.0.1", "3");
        assert_eq!(card_record_field(&r, "hostname"), "");
        assert_eq!(card_record_field(&r, "ip"), "");
        assert_eq!(card_record_field(&r, ""), "");
    }

    #[test]
    fn validate_match_key_rejects_missing_column() {
        let headers = vec!["host_ip".into(), "机房".into()];
        let result =
            validate_match_key_in_headers(&headers, &["nonexistent".into()], "test.csv");
        assert!(result.is_err(), "不存在的列名应被拒绝");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("nonexistent"), "错误信息应包含列名");
        assert!(msg.contains("host_ip"), "错误信息应列出可用列");
    }

    #[test]
    fn validate_match_key_rejects_missing_any_of_multi() {
        // 多键：任一键缺失即拒绝，错误信息列出全部缺失键
        let headers = vec!["host_ip".into(), "机房".into()];
        let keys = vec!["host_ip".into(), "card_id".into(), "node_name".into()];
        let result = validate_match_key_in_headers(&headers, &keys, "test.csv");
        assert!(result.is_err(), "任一匹配键缺失都应被拒绝");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("card_id"), "错误信息应包含缺失键 card_id");
        assert!(msg.contains("node_name"), "错误信息应包含缺失键 node_name");
        assert!(!msg.contains("host_ip、card_id、node_name"), "存在的 host_ip 不应列为缺失");
    }

    #[test]
    fn validate_match_key_rejects_empty() {
        let headers = vec!["host_ip".into()];
        let result = validate_match_key_in_headers(&headers, &[], "test.csv");
        assert!(result.is_err(), "空 match_keys 应被拒绝");
    }

    #[test]
    fn validate_match_key_accepts_existing_column() {
        let headers = vec!["host_ip".into(), "机房".into()];
        assert!(
            validate_match_key_in_headers(&headers, &["host_ip".into()], "test.csv").is_ok(),
            "存在的列名应通过校验"
        );
    }

    #[test]
    fn build_asset_index_duplicate_key_warnings() {
        let source = test_source(&["host_ip"], vec![]);
        let a1 = asset_row(&[("host_ip", "1.1.1.1"), ("机房", "北京A")], &source);
        let a2 = asset_row(&[("host_ip", "1.1.1.1"), ("机房", "北京B")], &source); // 重复 key
        let (index, warnings) = build_index(&[a1.clone(), a2.clone()], &source);
        assert_eq!(index.exact.as_ref().unwrap().len(), 1, "重复 key 应只保留首行");
        assert_eq!(warnings.len(), 1, "应有 1 条重复警告");
        assert!(warnings[0].contains("1.1.1.1"), "警告应包含重复 key");
        // 精确模式首行由索引决定：构造含机房列的索引验证取首行
        let source2 = test_source(
            &["host_ip"],
            vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
        );
        let (index2, _) = build_index(&[a1, a2], &source2);
        assert_eq!(
            join_record(&rec("1.1.1.1", "0"), &index2)
                .values
                .get("机房")
                .unwrap(),
            "北京A",
            "应保留首行"
        );
    }

    #[test]
    fn duplicate_rename_warnings_detects_cross_source_dupes() {
        let cfg = MappingConfig {
            enabled: true,
            sources: vec![
                MappingSource {
                    source_path: "a.csv".into(),
                    match_keys: vec!["host_ip".into()],
                    columns: vec![MappingColumn {
                        source_field: "room".into(),
                        rename: "机房".into(),
                        local_name: None,
                        position: InsertPosition::after("主机IP"),
                    }],
                    ..MappingSource::default()
                },
                MappingSource {
                    source_path: "b.csv".into(),
                    match_keys: vec!["host_ip".into()],
                    columns: vec![MappingColumn {
                        source_field: "location".into(),
                        rename: "机房".into(), // 与第一个来源重复
                        local_name: None,
                        position: InsertPosition::after("主机IP"),
                    }],
                    ..MappingSource::default()
                },
            ],
        };
        let warnings = cfg.duplicate_rename_warnings();
        assert_eq!(warnings.len(), 1, "跨来源的重复 rename 应被检测");
        assert!(warnings[0].contains("机房"), "警告应包含重复的 rename");
    }

    #[test]
    fn duplicate_rename_warnings_empty_for_unique_renames() {
        let cfg = MappingConfig {
            enabled: true,
            sources: vec![MappingSource {
                source_path: "a.csv".into(),
                match_keys: vec!["host_ip".into()],
                columns: vec![MappingColumn {
                    source_field: "room".into(),
                    rename: "机房".into(),
                    local_name: None,
                    position: InsertPosition::after("主机IP"),
                }],
                ..MappingSource::default()
            }],
        };
        assert!(
            cfg.duplicate_rename_warnings().is_empty(),
            "无重复 rename 不应产出警告"
        );
    }

    #[test]
    fn validate_result_columns_missing_key_and_source_field() {
        let source = test_source(
            &["host_ip"],
            vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
        );
        // 缺匹配键列
        let err = validate_result_columns(&["机房".into()], &source, "mysql://t").unwrap_err();
        assert!(format!("{err}").contains("host_ip"));
        // 缺注入列
        let err = validate_result_columns(&["host_ip".into()], &source, "mysql://t").unwrap_err();
        assert!(format!("{err}").contains("机房"));
        // 全部齐全
        assert!(
            validate_result_columns(&["host_ip".into(), "机房".into()], &source, "mysql://t")
                .is_ok()
        );
    }

    #[test]
    fn escape_identifier_doubles_backticks() {
        assert_eq!(escape_identifier("asset_tbl"), "asset_tbl");
        assert_eq!(escape_identifier("a`b"), "a``b");
    }

    #[test]
    fn compute_column_flags_host_handle_uses_logical_or() {
        // 两个设备类型：A 有 handle_expr，B 没有。
        // has_host_handle 应为 true（任一设备有即启用），不应被后者覆盖为 false。
        let sources = vec![crate::config::SourceConfig {
            name: "s".into(),
            url: "http://x".into(),
            timeout_secs: 30,
            device_types: vec!["dev_a".into(), "dev_b".into()],
        }];
        let mut devices = std::collections::HashMap::new();
        devices.insert("dev_a".into(), crate::devices::DeviceSpec {
            display_name: "A".into(),
            core_util_metric: "m".into(),
            memory: crate::devices::MemoryStrategy::composite_ratio("u", "f"),
            card_id_label: "gpu".into(),
            labels: crate::devices::LabelMapping {
                host_ip: "ip".into(),
                node_name: "n".into(),
                container: "c".into(),
                pod: "p".into(),
                namespace: "ns".into(),
            },
            temp_metric: None,
            power_metric: None,
            host_metrics: Some(crate::devices::HostMetricsSpec {
                enabled: true,
                cpu_expr: Some("cpu".into()),
                mem_expr: Some("mem".into()),
                handle_expr: Some("handle".into()),
                host_label: "instance".into(),
            }),
        });
        devices.insert("dev_b".into(), crate::devices::DeviceSpec {
            display_name: "B".into(),
            core_util_metric: "m2".into(),
            memory: crate::devices::MemoryStrategy::composite_ratio("u2", "f2"),
            card_id_label: "gpu".into(),
            labels: crate::devices::LabelMapping {
                host_ip: "ip".into(),
                node_name: "n".into(),
                container: "c".into(),
                pod: "p".into(),
                namespace: "ns".into(),
            },
            temp_metric: None,
            power_metric: None,
            host_metrics: Some(crate::devices::HostMetricsSpec {
                enabled: true,
                cpu_expr: Some("cpu2".into()),
                mem_expr: Some("mem2".into()),
                handle_expr: None,
                host_label: "instance".into(),
            }),
        });
        let flags = compute_column_flags(&sources, &devices);
        assert!(flags.has_host_handle, "has_host_handle 应为 true（dev_a 有 handle_expr），不应被 dev_b 覆盖");
        assert!(flags.has_host_cpu);
        assert!(flags.has_host_mem);
    }
}
