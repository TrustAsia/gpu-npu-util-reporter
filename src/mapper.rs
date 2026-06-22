//! 资产映射引擎模块。
//!
//! 职责二合一：(1) 加载外部资产表（CSV/Excel）并按 `match_key` 与每行
//! [`CardRecord`] 做 Join，注入资产字段；(2) 计算映射列在报表中的最终位置
//! （锚点列 + before/after 方向）。开关关闭时整个模块跳过。
//!
//! Join 设计：加载阶段为每行资产注入一个隐藏列 `@key`（由 `match_keys` 指定的
//! 资产列拼成），join 时把 `CardRecord` 同样字段拼成 key 直接比对，O(行数) 查找。
//!
//! 支持多来源映射：每个 `MappingSource` 可指定独立的资产表路径、匹配键和列映射，
//! 允许从不同资产表分别取值注入报表。

use crate::error::AppError;
use crate::processor::CardRecord;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 报表"基础列"的有序清单（与 reporter 共用）。
/// mapper 的锚点列名引用其中之一。
pub const BASE_COLUMNS: &[&str] = &[
    "数据来源",
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

/// 列插入位置：相对于某锚点列的前/后。
///
/// serde 表示为一个对象 `{ direction: before|after, anchor: <列名> }`，
/// 而非外部标记枚举——因为 `serde_yaml` 不支持默认的 externally-tagged 变体。
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
    /// 注入后的新列名。
    pub rename: String,
    /// 插入位置。
    pub position: InsertPosition,
}

/// 匹配键：从 `CardRecord` / 资产行取哪些字段拼 join key。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum MatchKey {
    HostIp,
    CardId,
    NodeName,
    SourceName,
    DeviceType,
    Namespace,
    Pod,
    Container,
}

/// 单个映射来源：独立的资产表路径 + 匹配键 + 列映射。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MappingSource {
    /// 资产表路径（按扩展名分流 CSV/Excel）。
    pub source_path: String,
    /// 该来源使用的匹配键。
    pub match_keys: Vec<MatchKey>,
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
    /// 收集所有来源的映射列（保持来源顺序，来源内部按配置顺序）。
    pub fn all_columns(&self) -> Vec<&MappingColumn> {
        self.sources.iter().flat_map(|s| &s.columns).collect()
    }

    /// 收集所有来源的映射列（owned clone），用于需要所有权的场景。
    pub fn all_columns_owned(&self) -> Vec<MappingColumn> {
        self.sources.iter().flat_map(|s| s.columns.clone()).collect()
    }
}

/// 资产表行：列名 → 值（含加载阶段注入的 `@key`）。
type AssetRow = HashMap<String, String>;

/// 资产表里 `match_key` 对应的列名。
const fn asset_key_label(k: &MatchKey) -> &'static str {
    match k {
        MatchKey::HostIp => "host_ip",
        MatchKey::CardId => "card_id",
        MatchKey::NodeName => "node_name",
        MatchKey::SourceName => "source_name",
        MatchKey::DeviceType => "device_type",
        MatchKey::Namespace => "namespace",
        MatchKey::Pod => "pod",
        MatchKey::Container => "container",
    }
}

/// 为一行资产注入 `@key`（由 `match_keys` 指定的列拼成）。
fn inject_key(row: &mut AssetRow, match_keys: &[MatchKey]) {
    let key = match_keys
        .iter()
        .map(|k| row.get(asset_key_label(k)).cloned().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("|");
    row.insert("@key".into(), key);
}

/// 由一张卡的字段构造 join key 字符串。
fn join_key(rec: &CardRecord, keys: &[MatchKey]) -> String {
    keys.iter()
        .map(|k| match k {
            MatchKey::HostIp => rec.host_ip.clone(),
            MatchKey::CardId => rec.card_id.clone(),
            MatchKey::NodeName => rec.node_name.clone(),
            MatchKey::SourceName => rec.source_name.clone(),
            MatchKey::DeviceType => rec.device_type.clone(),
            MatchKey::Namespace => rec.namespace.clone(),
            MatchKey::Pod => rec.pod.clone(),
            MatchKey::Container => rec.container.clone(),
        })
        .collect::<Vec<_>>()
        .join("|")
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
    let mut placements: Vec<(usize, String)> = mapping_cols
        .iter()
        .map(|c| {
            let target = base
                .iter()
                .position(|x| *x == c.position.anchor)
                .map_or(result.len(), |idx| match c.position.direction {
                    Direction::Before => idx,
                    Direction::After => idx + 1,
                });
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

/// 加载资产表，并为每行注入 `@key`（由 `match_keys` 指定的列拼成）。
/// 按扩展名分流：`.csv` 用 csv crate，`.xlsx`/`.xls`/`.xlsb`/`.ods` 用 calamine 自动检测。
/// 首行视为表头。
///
/// # Errors
///
/// 返回 [`AppError::Mapping`] 当文件读取/解析失败或格式不支持。
pub fn load_asset_table(path: &str, match_keys: &[MatchKey]) -> Result<Vec<AssetRow>, AppError> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext == "csv" {
        load_csv(path, match_keys)
    } else if matches!(ext.as_str(), "xlsx" | "xls" | "xlsb" | "ods") {
        load_excel(path, match_keys)
    } else {
        Err(AppError::Mapping {
            path: path.into(),
            detail: "不支持的资产表格式（仅支持 .csv/.xlsx/.xls/.xlsb/.ods）".into(),
        })
    }
}

fn load_csv(path: &str, match_keys: &[MatchKey]) -> Result<Vec<AssetRow>, AppError> {
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
    let mut rows = Vec::new();
    for rec in rdr.records() {
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

fn load_excel(path: &str, match_keys: &[MatchKey]) -> Result<Vec<AssetRow>, AppError> {
    use calamine::{open_workbook_auto, Reader, Sheets};
    let mut book: Sheets<_> = open_workbook_auto(path).map_err(|e| AppError::Mapping {
        path: path.into(),
        detail: format!("打开 Excel 失败：{e}"),
    })?;
    let name = book
        .sheet_names()
        .first()
        .cloned()
        .ok_or_else(|| AppError::Mapping {
            path: path.into(),
            detail: "Excel 无工作表".into(),
        })?;
    let range = book.worksheet_range(&name).map_err(|e| AppError::Mapping {
        path: path.into(),
        detail: format!("读取工作表失败：{e}"),
    })?;
    let mut iter = range.rows();
    let header = iter.next().ok_or_else(|| AppError::Mapping {
        path: path.into(),
        detail: "Excel 首行（表头）为空".into(),
    })?;
    let headers: Vec<String> = header.iter().map(ToString::to_string).collect();
    let mut rows = Vec::new();
    for row in iter {
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
/// 同一 `@key` 出现多次时取首行（与原线性扫描行为一致）。
#[must_use]
pub fn build_asset_index(assets: &[AssetRow]) -> AssetIndex {
    let mut idx = HashMap::with_capacity(assets.len());
    for row in assets {
        if let Some(key) = row.get("@key") {
            idx.entry(key.clone()).or_insert_with(|| row.clone());
        }
    }
    idx
}

/// 对一行 `CardRecord` 做单来源 join，返回 (rename → value)。
/// 未命中返回空 map（调用方记 Warning）。
#[must_use]
pub fn join_record(
    rec: &CardRecord,
    index: &AssetIndex,
    source: &MappingSource,
) -> HashMap<String, String> {
    let key = join_key(rec, &source.match_keys);
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
                position: InsertPosition::after("主机IP"),
            },
            MappingColumn {
                source_field: "负责人".into(),
                rename: "负责人".into(),
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
            position: InsertPosition::after("不存在"),
        }];
        let order = compute_column_order(BASE_COLUMNS, &cols);
        assert_eq!(order.last().unwrap(), "X");
    }

    #[test]
    fn join_record_hits_and_misses() {
        let source = MappingSource {
            source_path: String::new(),
            match_keys: vec![MatchKey::HostIp, MatchKey::CardId],
            columns: vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                position: InsertPosition::after("主机IP"),
            }],
        };
        let mut a1 = HashMap::new();
        a1.insert("host_ip".into(), "1.1.1.1".into());
        a1.insert("card_id".into(), "0".into());
        a1.insert("机房".into(), "北京A".into());
        inject_key(&mut a1, &source.match_keys);
        let assets = vec![a1];
        let index = build_asset_index(&assets);

        let hit = join_record(&rec("1.1.1.1", "0"), &index, &source);
        assert_eq!(hit.get("机房").unwrap(), "北京A");

        let miss = join_record(&rec("2.2.2.2", "0"), &index, &source);
        assert!(miss.is_empty());
    }

    #[test]
    fn join_record_with_source_name_key() {
        let source = MappingSource {
            source_path: String::new(),
            match_keys: vec![MatchKey::SourceName, MatchKey::HostIp],
            columns: vec![MappingColumn {
                source_field: "集群".into(),
                rename: "集群".into(),
                position: InsertPosition::after("数据来源"),
            }],
        };
        let mut a1 = HashMap::new();
        a1.insert("source_name".into(), "s".into());
        a1.insert("host_ip".into(), "1.1.1.1".into());
        a1.insert("集群".into(), "生产集群".into());
        inject_key(&mut a1, &source.match_keys);
        let assets = vec![a1];
        let index = build_asset_index(&assets);

        let hit = join_record(&rec("1.1.1.1", "0"), &index, &source);
        assert_eq!(hit.get("集群").unwrap(), "生产集群");
    }

    #[test]
    fn multi_source_mapping() {
        // 两个来源：机房表用 host_ip+card_id 匹配，负责人表只用 host_ip 匹配
        let src_room = MappingSource {
            source_path: String::new(),
            match_keys: vec![MatchKey::HostIp, MatchKey::CardId],
            columns: vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                position: InsertPosition::after("主机IP"),
            }],
        };
        let src_owner = MappingSource {
            source_path: String::new(),
            match_keys: vec![MatchKey::HostIp],
            columns: vec![MappingColumn {
                source_field: "负责人".into(),
                rename: "负责人".into(),
                position: InsertPosition::after("机房"),
            }],
        };

        // 机房表
        let mut a1 = HashMap::new();
        a1.insert("host_ip".into(), "1.1.1.1".into());
        a1.insert("card_id".into(), "0".into());
        a1.insert("机房".into(), "北京A".into());
        inject_key(&mut a1, &src_room.match_keys);
        let room_index = build_asset_index(&[a1]);

        // 负责人表
        let mut a2 = HashMap::new();
        a2.insert("host_ip".into(), "1.1.1.1".into());
        a2.insert("负责人".into(), "张三".into());
        inject_key(&mut a2, &src_owner.match_keys);
        let owner_index = build_asset_index(&[a2]);

        let r = rec("1.1.1.1", "0");
        let room_vals = join_record(&r, &room_index, &src_room);
        let owner_vals = join_record(&r, &owner_index, &src_owner);
        assert_eq!(room_vals.get("机房").unwrap(), "北京A");
        assert_eq!(owner_vals.get("负责人").unwrap(), "张三");
    }

    #[test]
    fn missing_anchor_warnings_reports_non_base_anchors() {
        // PRD §2.3：锚点必须是基础列。一个合法 + 一个非法锚点。
        let cols = vec![
            MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                position: InsertPosition::after("主机IP"), // 合法（基础列）
            },
            MappingColumn {
                source_field: "x".into(),
                rename: "X".into(),
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
            position: InsertPosition::before("设备类型"),
        }];
        assert!(missing_anchor_warnings(BASE_COLUMNS, &cols).is_empty());
    }
}
