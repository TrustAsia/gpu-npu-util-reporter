//! 资产映射引擎模块。
//!
//! 职责二合一：(1) 加载外部资产表（CSV/Excel）并按 `match_keys` 与每行
//! [`CardRecord`] 做 Join，注入资产字段；(2) 计算映射列在报表中的最终位置
//! （锚点列 + before/after 方向）。开关关闭时整个模块跳过。
//!
//! Join 设计：加载阶段为每行资产注入一个隐藏列 `@key`（由 `match_keys` 指定的
//! 资产列值），join 时把 `CardRecord` 同样字段拼成 key 直接比对，O(行数) 查找。
//!
//! 支持多来源映射：每个 `MappingSource` 可指定独立的资产表路径、匹配键和列映射，
//! 允许从不同资产表分别取值注入报表。
//!
//! `match_keys` 为字符串，直接指定资产表中的列名。CardRecord 侧通过
//! [`card_record_field`] 函数将已知的字段名映射到对应字段值，支持：
//! `source_name`、`host_ip`、`node_name`、`card_id`、`device_type`、
//! `namespace`、`pod`、`container`。不在上述列表中的 `match_keys` 在资产表
//! 侧仍可正常拼 key，但 `CardRecord` 侧取值为空串（join 不会命中）。

use crate::error::AppError;
use crate::processor::CardRecord;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 报表核心基础列（始终出现）。
const CORE_BASE_COLUMNS: &[&str] = &[
    "数据来源",
    "主机IP",
    "节点名称",
    "计算卡编号",
    "设备类型",
    "Namespace",
    "Pod",
    "容器名称",
    "数据开始时间",
    "数据结束时间",
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
    "host_ip",
    "node_name",
    "card_id",
    "device_type",
    "namespace",
    "pod",
    "container",
    "range_start",
    "range_end",
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
    #[allow(dead_code)]
    pub fn before(anchor: impl Into<String>) -> Self {
        Self {
            direction: Direction::Before,
            anchor: anchor.into(),
        }
    }
    /// 便捷构造：锚点列之后。
    #[allow(dead_code)]
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

/// 单个映射来源：独立的资产表路径 + 匹配键 + 列映射。
///
/// `match_keys` 为字符串，指定资产表中的匹配列名。CardRecord 侧通过
/// `record_key`（可选）指定对应字段名；不指定时默认与 `match_keys` 相同。
/// [`card_record_field`] 支持的字段名：`source_name`、`host_ip`、`node_name`、
/// `card_id`、`device_type`、`namespace`、`pod`、`container`。
/// 不在已知列表中的字段名在 `CardRecord` 侧取值为空串。
///
/// 可选 `source_sheet` 指定 Excel 工作表名；不指定时取第一个工作表。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MappingSource {
    /// 资产表路径（按扩展名分流 CSV/Excel）。
    pub source_path: String,
    /// 可选 Excel 工作表名；不指定时取第一个工作表。
    #[serde(default)]
    pub source_sheet: Option<String>,
    /// 资产表中的匹配列名。
    ///
    /// `CardRecord` 侧通过 `record_key` 映射对应字段；不指定 `record_key` 时
    /// 默认与 `match_keys` 相同。
    pub match_keys: String,
    /// `CardRecord` 侧对应的字段名（可选）。
    ///
    /// 支持的字段名：`source_name`、`host_ip`、`node_name`、`card_id`、
    /// `device_type`、`namespace`、`pod`、`container`。
    /// 不指定时默认与 `match_keys` 相同，适用于资产表列名与 `CardRecord`
    /// 字段名一致的场景（如 `host_ip`）。
    /// 当资产表列名不同于 `CardRecord` 字段名时（如资产表用 `IP地址`，
    /// `CardRecord` 用 `host_ip`），需要显式指定 `record_key`。
    #[serde(default)]
    pub record_key: Option<String>,
    /// 从该资产表提取的列映射。
    pub columns: Vec<MappingColumn>,
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

/// 为一行资产注入 `@key`（由 `match_keys` 指定的列拼成）。
fn inject_key(row: &mut AssetRow, match_keys: &str) {
    let key = row.get(match_keys).cloned().unwrap_or_default();
    row.insert("@key".into(), key);
}

/// 由一张卡的字段构造 join key 字符串。
/// 使用 `record_key`（有值时）或 `match_keys`（默认）作为 `CardRecord` 字段名。
fn join_key(rec: &CardRecord, source: &MappingSource) -> String {
    let field = source.record_key.as_deref().unwrap_or(&source.match_keys);
    card_record_field(rec, field)
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

/// 加载资产表，并为每行注入 `@key`（由 `match_keys` 指定的列拼成）。
/// 按扩展名分流：`.csv` 用 csv crate，`.xlsx`/`.xls`/`.xlsb`/`.ods` 用 calamine 自动检测。
/// 首行视为表头。
///
/// # Errors
///
/// 返回 [`AppError::Mapping`] 当文件读取/解析失败或格式不支持。
pub fn load_asset_table(
    path: &str,
    match_keys: &str,
    sheet: Option<&str>,
) -> Result<Vec<AssetRow>, AppError> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "csv" {
        load_csv(path, match_keys)
    } else if matches!(ext.as_str(), "xlsx" | "xls" | "xlsb" | "ods") {
        load_excel(path, match_keys, sheet)
    } else {
        Err(AppError::Mapping {
            path: path.into(),
            detail: "不支持的资产表格式（仅支持 .csv/.xlsx/.xls/.xlsb/.ods）".into(),
        })
    }
}

/// 检查 `match_keys` 列是否存在于表头中，不存在时返回错误。
fn validate_match_key_in_headers(
    headers: &[String],
    match_keys: &str,
    path: &str,
) -> Result<(), AppError> {
    if match_keys.is_empty() {
        return Err(AppError::Mapping {
            path: path.into(),
            detail: "match_keys 不能为空字符串".into(),
        });
    }
    if !headers.iter().any(|h| h == match_keys) {
        return Err(AppError::Mapping {
            path: path.into(),
            detail: format!(
                "match_keys「{match_keys}」在资产表表头中不存在（可用列：{}）",
                headers.join(", ")
            ),
        });
    }
    Ok(())
}

fn load_csv(path: &str, match_keys: &str) -> Result<Vec<AssetRow>, AppError> {
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
        inject_key(&mut row, match_keys);
        rows.push(row);
    }
    Ok(rows)
}

fn load_excel(
    path: &str,
    match_keys: &str,
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
        inject_key(&mut m, match_keys);
        rows.push(m);
    }
    Ok(rows)
}

/// 资产表索引：`@key` → 资产行。由 [`build_asset_index`] 构建，供 [`join_record`] 做 O(1) 查找。
type AssetIndex = HashMap<String, AssetRow>;

/// 从资产行列表构建 `@key` 索引，供 `join_record` 做 O(1) 查找。
/// 同一 `@key` 出现多次时取首行，并返回重复 key 的警告列表。
#[must_use]
pub fn build_asset_index(assets: &[AssetRow]) -> (AssetIndex, Vec<String>) {
    let mut idx = HashMap::with_capacity(assets.len());
    let mut warnings = Vec::new();
    for row in assets {
        if let Some(key) = row.get("@key") {
            if idx.contains_key(key) {
                warnings.push(format!(
                    "资产表 @key「{key}」重复，仅保留首行（跳过后续重复行）"
                ));
            } else {
                idx.insert(key.clone(), row.clone());
            }
        }
    }
    (idx, warnings)
}

/// 对一行 `CardRecord` 做单来源 join，返回 (rename → value)。
/// 未命中返回空 map（调用方记 Warning）。
#[must_use]
pub fn join_record(
    rec: &CardRecord,
    index: &AssetIndex,
    source: &MappingSource,
) -> HashMap<String, String> {
    let key = join_key(rec, source);
    let mut out = HashMap::new();
    if let Some(row) = index.get(&key) {
        for col in &source.columns {
            if let Some(v) = row.get(&col.source_field) {
                out.insert(col.rename.clone(), v.clone());
            }
        }
    }
    out
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

    #[test]
    fn join_record_hits_and_misses() {
        let source = MappingSource {
            source_path: String::new(),
            source_sheet: None,
            match_keys: "host_ip".into(),
            record_key: None,
            columns: vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
        };
        let mut a1 = HashMap::new();
        a1.insert("host_ip".into(), "1.1.1.1".into());
        a1.insert("机房".into(), "北京A".into());
        inject_key(&mut a1, &source.match_keys);
        let assets = vec![a1];
        let (index, _) = build_asset_index(&assets);

        let hit = join_record(&rec("1.1.1.1", "0"), &index, &source);
        assert_eq!(hit.get("机房").unwrap(), "北京A");

        let miss = join_record(&rec("2.2.2.2", "0"), &index, &source);
        assert!(miss.is_empty());
    }

    #[test]
    fn join_record_with_custom_key_name() {
        // 资产表用 "IP地址" 作为匹配列，CardRecord 用 "host_ip"
        let source = MappingSource {
            source_path: String::new(),
            source_sheet: None,
            match_keys: "IP地址".into(),
            record_key: Some("host_ip".into()),
            columns: vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
        };
        let mut a1 = HashMap::new();
        a1.insert("IP地址".into(), "1.1.1.1".into());
        a1.insert("机房".into(), "北京A".into());
        inject_key(&mut a1, &source.match_keys);
        let assets = vec![a1];
        let (index, _) = build_asset_index(&assets);

        // CardRecord 的 host_ip 字段值 "1.1.1.1" 通过 record_key 映射，
        // 应能匹配到资产表的 "IP地址" 列值
        let hit = join_record(&rec("1.1.1.1", "0"), &index, &source);
        assert_eq!(hit.get("机房").unwrap(), "北京A");
    }

    #[test]
    fn join_record_with_unknown_key_returns_empty() {
        // match_keys 指定了 CardRecord 不存在的字段名 → join key 为空串 → 不会命中
        let source = MappingSource {
            source_path: String::new(),
            source_sheet: None,
            match_keys: "unknown_column".into(),
            record_key: None,
            columns: vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
        };
        let mut a1 = HashMap::new();
        a1.insert("unknown_column".into(), "1.1.1.1".into());
        a1.insert("机房".into(), "北京A".into());
        inject_key(&mut a1, &source.match_keys);
        let assets = vec![a1];
        let (index, _) = build_asset_index(&assets);

        let miss = join_record(&rec("1.1.1.1", "0"), &index, &source);
        assert!(
            miss.is_empty(),
            "未知字段名应导致 join key 为空串，不会命中"
        );
    }

    #[test]
    fn multi_source_mapping() {
        // 两个来源：机房表用 host_ip 匹配，负责人表用 node_name 匹配
        let src_room = MappingSource {
            source_path: String::new(),
            source_sheet: None,
            match_keys: "host_ip".into(),
            record_key: None,
            columns: vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                local_name: None,
                position: InsertPosition::after("主机IP"),
            }],
        };
        let src_owner = MappingSource {
            source_path: String::new(),
            source_sheet: None,
            match_keys: "node_name".into(),
            record_key: None,
            columns: vec![MappingColumn {
                source_field: "负责人".into(),
                rename: "负责人".into(),
                local_name: None,
                position: InsertPosition::after("机房"),
            }],
        };

        // 机房表
        let mut a1 = HashMap::new();
        a1.insert("host_ip".into(), "1.1.1.1".into());
        a1.insert("机房".into(), "北京A".into());
        inject_key(&mut a1, &src_room.match_keys);
        let (room_index, _) = build_asset_index(&[a1]);

        // 负责人表
        let mut a2 = HashMap::new();
        a2.insert("node_name".into(), "node-1".into());
        a2.insert("负责人".into(), "张三".into());
        inject_key(&mut a2, &src_owner.match_keys);
        let (owner_index, _) = build_asset_index(&[a2]);

        let mut r = rec("1.1.1.1", "0");
        r.node_name = "node-1".into();
        let room_vals = join_record(&r, &room_index, &src_room);
        assert_eq!(room_vals.get("机房").unwrap(), "北京A");
        let owner_vals = join_record(&r, &owner_index, &src_owner);
        assert_eq!(owner_vals.get("负责人").unwrap(), "张三");
    }

    #[test]
    fn multi_source_with_custom_record_key() {
        // 资产表用 "主机名" 列名，CardRecord 用 node_name 字段
        let src_owner = MappingSource {
            source_path: String::new(),
            source_sheet: None,
            match_keys: "主机名".into(),
            record_key: Some("node_name".into()),
            columns: vec![MappingColumn {
                source_field: "负责人".into(),
                rename: "负责人".into(),
                local_name: None,
                position: InsertPosition::after("机房"),
            }],
        };

        let mut a2 = HashMap::new();
        a2.insert("主机名".into(), "node-1".into());
        a2.insert("负责人".into(), "张三".into());
        inject_key(&mut a2, &src_owner.match_keys);
        let (owner_index, _) = build_asset_index(&[a2]);

        let mut r = rec("1.1.1.1", "0");
        r.node_name = "node-1".into();
        let owner_vals = join_record(&r, &owner_index, &src_owner);
        assert_eq!(owner_vals.get("负责人").unwrap(), "张三");
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
        let result = validate_match_key_in_headers(&headers, "nonexistent", "test.csv");
        assert!(result.is_err(), "不存在的列名应被拒绝");
        let msg = format!("{}", result.unwrap_err());
        assert!(msg.contains("nonexistent"), "错误信息应包含列名");
        assert!(msg.contains("host_ip"), "错误信息应列出可用列");
    }

    #[test]
    fn validate_match_key_rejects_empty() {
        let headers = vec!["host_ip".into()];
        let result = validate_match_key_in_headers(&headers, "", "test.csv");
        assert!(result.is_err(), "空 match_keys 应被拒绝");
    }

    #[test]
    fn validate_match_key_accepts_existing_column() {
        let headers = vec!["host_ip".into(), "机房".into()];
        assert!(
            validate_match_key_in_headers(&headers, "host_ip", "test.csv").is_ok(),
            "存在的列名应通过校验"
        );
    }

    #[test]
    fn build_asset_index_duplicate_key_warnings() {
        let mut a1 = HashMap::new();
        a1.insert("host_ip".into(), "1.1.1.1".into());
        a1.insert("机房".into(), "北京A".into());
        inject_key(&mut a1, "host_ip");
        let mut a2 = HashMap::new();
        a2.insert("host_ip".into(), "1.1.1.1".into()); // 重复 key
        a2.insert("机房".into(), "北京B".into());
        inject_key(&mut a2, "host_ip");
        let (index, warnings) = build_asset_index(&[a1, a2]);
        assert_eq!(index.len(), 1, "重复 key 应只保留首行");
        assert_eq!(warnings.len(), 1, "应有 1 条重复警告");
        assert!(warnings[0].contains("1.1.1.1"), "警告应包含重复 key");
        assert_eq!(
            index.get("1.1.1.1").unwrap().get("机房").unwrap(),
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
                    source_sheet: None,
                    match_keys: "host_ip".into(),
                    record_key: None,
                    columns: vec![MappingColumn {
                        source_field: "room".into(),
                        rename: "机房".into(),
                        local_name: None,
                        position: InsertPosition::after("主机IP"),
                    }],
                },
                MappingSource {
                    source_path: "b.csv".into(),
                    source_sheet: None,
                    match_keys: "host_ip".into(),
                    record_key: None,
                    columns: vec![MappingColumn {
                        source_field: "location".into(),
                        rename: "机房".into(), // 与第一个来源重复
                        local_name: None,
                        position: InsertPosition::after("主机IP"),
                    }],
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
                source_sheet: None,
                match_keys: "host_ip".into(),
                record_key: None,
                columns: vec![MappingColumn {
                    source_field: "room".into(),
                    rename: "机房".into(),
                    local_name: None,
                    position: InsertPosition::after("主机IP"),
                }],
            }],
        };
        assert!(
            cfg.duplicate_rename_warnings().is_empty(),
            "无重复 rename 不应产出警告"
        );
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
