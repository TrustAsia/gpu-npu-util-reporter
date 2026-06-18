//! 资产映射引擎模块。
//!
//! 职责二合一：(1) 加载外部资产表（CSV/Excel）并按 match_key 与每行
//! [`CardRecord`] 做 Join，注入资产字段；(2) 计算映射列在报表中的最终位置
//! （锚点列 + before/after 方向）。开关关闭时整个模块跳过。
//!
//! Join 设计：加载阶段为每行资产注入一个隐藏列 `@key`（由 match_keys 指定的
//! 资产列拼成），join 时把 CardRecord 同样字段拼成 key 直接比对，O(行数) 查找。

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
    "显存占用率平均值",
    "显存占用率峰值",
    "显存占用率峰值出现时间",
];

/// 列插入位置：相对于某锚点列的前/后。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum InsertPosition {
    /// 锚点列之前。
    Before(String),
    /// 锚点列之后。
    After(String),
}

/// 单个映射列的配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MappingColumn {
    /// 资产表源列名。
    pub source_field: String,
    /// 注入后的新列名。
    pub rename: String,
    /// 插入位置。
    pub position: InsertPosition,
}

/// 匹配键：从 CardRecord / 资产行取哪些字段拼 join key。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MatchKey {
    HostIp,
    CardId,
    NodeName,
}

/// 资产映射总配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MappingConfig {
    pub enabled: bool,
    /// 资产表路径（按扩展名分流 CSV/Excel）。
    pub source_path: String,
    pub match_keys: Vec<MatchKey>,
    pub columns: Vec<MappingColumn>,
}

/// 资产表行：列名 → 值（含加载阶段注入的 `@key`）。
type AssetRow = HashMap<String, String>;

/// 资产表里 match_key 对应的列名（约定与 CardRecord 字段同名）。
fn asset_key_label(k: &MatchKey) -> &'static str {
    match k {
        MatchKey::HostIp => "host_ip",
        MatchKey::CardId => "card_id",
        MatchKey::NodeName => "node_name",
    }
}

/// 为一行资产注入 `@key`（由 match_keys 指定的列拼成）。
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
        })
        .collect::<Vec<_>>()
        .join("|")
}

/// 计算最终列顺序：基础列 + 按 position 插入的映射列。
///
/// 算法：按配置顺序逐个插入映射列。每插入一列，后续列就能锚定到它
/// （支持链式锚定，如 机房锚主机IP、负责人锚机房）。每列的目标位置由
/// 当前 `result` 数组解析：Before(X)→X 的当前 index，After(X)→该 index + 1。
/// 锚点不存在时该列追加到末尾。
pub fn compute_column_order(
    base: &[&str],
    mapping_cols: &[MappingColumn],
) -> Vec<String> {
    let mut result: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    for c in mapping_cols {
        let anchor = match &c.position {
            InsertPosition::Before(a) => a,
            InsertPosition::After(a) => a,
        };
        let target = match result.iter().position(|x| x == anchor) {
            Some(idx) => match c.position {
                InsertPosition::Before(_) => idx,
                InsertPosition::After(_) => idx + 1,
            },
            None => result.len(),
        };
        let insert_at = target.min(result.len());
        result.insert(insert_at, c.rename.clone());
    }
    result
}

/// 加载资产表，并为每行注入 `@key`（由 match_keys 指定的列拼成）。
/// 按扩展名分流：`.csv` 用 csv crate，`.xlsx`/`.xls` 用 calamine。首行视为表头。
pub fn load_asset_table(
    path: &str,
    match_keys: &[MatchKey],
) -> Result<Vec<AssetRow>, AppError> {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".csv") {
        load_csv(path, match_keys)
    } else if lower.ends_with(".xlsx") || lower.ends_with(".xls") {
        load_xlsx(path, match_keys)
    } else {
        Err(AppError::Mapping {
            path: path.into(),
            detail: "不支持的资产表格式（仅支持 .csv/.xlsx）".into(),
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
        .map(|s| s.to_string())
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

fn load_xlsx(path: &str, match_keys: &[MatchKey]) -> Result<Vec<AssetRow>, AppError> {
    use calamine::{open_workbook, Reader, Xlsx};
    let mut book: Xlsx<_> = open_workbook(path).map_err(|e| AppError::Mapping {
        path: path.into(),
        detail: format!("打开 Excel 失败：{e}"),
    })?;
    let name = book
        .sheet_names()
        .get(0)
        .cloned()
        .ok_or_else(|| AppError::Mapping {
            path: path.into(),
            detail: "Excel 无工作表".into(),
        })?;
    let range = book
        .worksheet_range(&name)
        .map_err(|e| AppError::Mapping {
            path: path.into(),
            detail: format!("读取工作表失败：{e}"),
        })?;
    let mut iter = range.rows();
    let header = iter.next().ok_or_else(|| AppError::Mapping {
        path: path.into(),
        detail: "Excel 首行（表头）为空".into(),
    })?;
    let headers: Vec<String> = header.iter().map(|c| c.to_string()).collect();
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

/// 对一行 CardRecord 做 join，返回 (rename → value)。
/// 未命中返回空 map（调用方记 Warning）。
pub fn join_record(
    rec: &CardRecord,
    assets: &[AssetRow],
    cfg: &MappingConfig,
) -> HashMap<String, String> {
    let key = join_key(rec, &cfg.match_keys);
    let mut out = HashMap::new();
    for row in assets {
        if row.get("@key").map(|s| s.as_str()) == Some(key.as_str()) {
            for col in &cfg.columns {
                if let Some(v) = row.get(&col.source_field) {
                    out.insert(col.rename.clone(), v.clone());
                }
            }
            return out;
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
            node_name: "".into(),
            card_id: card.into(),
            device_type: "X".into(),
            namespace: "".into(),
            pod: "".into(),
            container: "".into(),
            core_avg: None,
            core_peak: None,
            core_peak_time: None,
            mem_avg: None,
            mem_peak: None,
            mem_peak_time: None,
            range_start: Utc.timestamp_opt(0, 0).unwrap(),
            range_end: Utc.timestamp_opt(60, 0).unwrap(),
        }
    }

    #[test]
    fn column_order_inserts_after_anchor() {
        let cols = vec![
            MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                position: InsertPosition::After("主机IP".into()),
            },
            MappingColumn {
                source_field: "负责人".into(),
                rename: "负责人".into(),
                position: InsertPosition::After("机房".into()),
            },
        ];
        let order = compute_column_order(BASE_COLUMNS, &cols);
        let ip = order.iter().position(|s| s == "主机IP").unwrap();
        let room = order.iter().position(|s| s == "机房").unwrap();
        let owner = order.iter().position(|s| s == "负责人").unwrap();
        assert_eq!(room, ip + 1);
        assert_eq!(owner, room + 1);
    }

    #[test]
    fn column_order_before_anchor() {
        let cols = vec![MappingColumn {
            source_field: "x".into(),
            rename: "X".into(),
            position: InsertPosition::Before("设备类型".into()),
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
            position: InsertPosition::After("不存在".into()),
        }];
        let order = compute_column_order(BASE_COLUMNS, &cols);
        assert_eq!(order.last().unwrap(), "X");
    }

    #[test]
    fn join_record_hits_and_misses() {
        let cfg = MappingConfig {
            enabled: true,
            source_path: "".into(),
            match_keys: vec![MatchKey::HostIp, MatchKey::CardId],
            columns: vec![MappingColumn {
                source_field: "机房".into(),
                rename: "机房".into(),
                position: InsertPosition::After("主机IP".into()),
            }],
        };
        let mut a1 = HashMap::new();
        a1.insert("host_ip".into(), "1.1.1.1".into());
        a1.insert("card_id".into(), "0".into());
        a1.insert("机房".into(), "北京A".into());
        inject_key(&mut a1, &cfg.match_keys);
        let assets = vec![a1];

        let hit = join_record(&rec("1.1.1.1", "0"), &assets, &cfg);
        assert_eq!(hit.get("机房").unwrap(), "北京A");

        let miss = join_record(&rec("2.2.2.2", "0"), &assets, &cfg);
        assert!(miss.is_empty());
    }
}
