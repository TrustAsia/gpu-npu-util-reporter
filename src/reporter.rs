//! 报告渲染层模块。
//!
//! 隔离 rust_xlsxwriter，专职把 `Vec<CardRecord>` + 列布局 + 染色决策写成带样式的
//! `.xlsx`。基础列顺序来自 [`mapper::BASE_COLUMNS`](crate::mapper::BASE_COLUMNS)，
//! 映射列位置由 mapper 计算后传入；阈值染色由 highlight 计算命中后传入颜色。

use crate::error::AppError;
use crate::highlight::{HexColor, ThresholdTriggers};
use crate::mapper::compute_column_order;
use crate::mapper::MappingColumn;
use crate::processor::CardRecord;
use rust_xlsxwriter::{Color, Format, Workbook};
use std::collections::HashMap;

/// 报表列规格：基础列清单 + 映射列 rename（用于校验/未来扩展）。
pub struct ReportSpec {
    pub base_columns: Vec<String>,
    /// 映射列 rename 清单（当前未直接参与渲染，列顺序已由 compute_column_order 决定）。
    pub mapping_renames: Vec<String>,
}

/// 把记录写为 .xlsx 字节缓冲。
///
/// - 首行冻结 + 加粗 + 深蓝底白字
/// - 利用率列存为 value/100，数字格式 0.00%；N/A 写字符串 "N/A"
/// - 时间列以 `YYYY-MM-DD HH:MM:SS` 文本输出（PRD §3 显示要求；
///   规避 rust_xlsxwriter 0.79 中 chrono 不实现 IntoExcelDateTime 的限制）
/// - 命中染色单元格套对应 HEX 背景色 Format
pub fn render_to_buffer(
    records: &[CardRecord],
    spec: &ReportSpec,
    mapping_columns: &[MappingColumn],
    thresholds: &ThresholdTriggers,
    mapping_values: &HashMap<(usize, String), String>, // (行索引, rename) -> 资产值
) -> Result<Vec<u8>, AppError> {
    // 让 mapping_renames 不触发未使用警告（保留字段以备未来校验）。
    let _ = &spec.mapping_renames;

    let base_refs: Vec<&str> = spec.base_columns.iter().map(|s| s.as_str()).collect();
    let order = compute_column_order(&base_refs, mapping_columns);

    let mut wb = Workbook::new();
    let sheet = wb
        .add_worksheet()
        .set_name("利用率报表")
        .map_err(|e| AppError::Report {
            detail: format!("设置工作表名失败：{e}"),
        })?;

    let header_fmt = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0x1F4E79))
        .set_font_color(Color::White);
    let pct_fmt = Format::new().set_num_format("0.00%");

    // 列名 → 列索引
    let col_index: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i))
        .collect();

    // 写表头
    for (i, name) in order.iter().enumerate() {
        sheet
            .write_string_with_format(0, i as u16, name, &header_fmt)
            .map_err(|e| AppError::Report {
                detail: format!("写表头失败：{e}"),
            })?;
    }
    sheet
        .set_freeze_panes(1, 0)
        .map_err(|e| AppError::Report {
            detail: format!("冻结首行失败：{e}"),
        })?;

    // 命中染色单元格的百分比 Format 构造器（按 HEX）
    let pct_color = |hex: &HexColor| -> Format {
        let rgb = u32::from_str_radix(&hex.0[1..], 16).unwrap_or(0xFF0000);
        Format::new()
            .set_background_color(Color::RGB(rgb))
            .set_num_format("0.00%")
    };

    for (row_idx, rec) in records.iter().enumerate() {
        let excel_row = (row_idx + 1) as u32;
        // 计算该行染色决策（列名 → HEX）
        let hits = thresholds.evaluate_row(rec);
        let hit_colors: HashMap<&str, &HexColor> =
            hits.iter().map(|h| (h.column, h.color)).collect();

        for name in &order {
            let col = *col_index.get(name.as_str()).unwrap_or(&0) as u16;
            let hit_color = hit_colors.get(name.as_str()).copied();
            let v = cell_value(rec, name, mapping_values, row_idx);
            match v {
                CellValue::Pct(p) => {
                    let fmt = match hit_color {
                        Some(hex) => pct_color(hex),
                        None => pct_fmt.clone(),
                    };
                    sheet
                        .write_number_with_format(excel_row, col, p / 100.0, &fmt)
                        .map_err(|e| AppError::Report {
                            detail: format!("写单元格失败：{e}"),
                        })?;
                }
                CellValue::Text(t) => {
                    sheet
                        .write_string(excel_row, col, t)
                        .map_err(|e| AppError::Report {
                            detail: format!("写文本失败：{e}"),
                        })?;
                }
                CellValue::Na => {
                    sheet
                        .write_string(excel_row, col, "N/A")
                        .map_err(|e| AppError::Report {
                            detail: format!("写 N/A 失败：{e}"),
                        })?;
                }
            }
        }
    }

    // 列宽自适应（按列名长度估算，clamp 到 [10, 40]）
    for (i, name) in order.iter().enumerate() {
        let width = (name.chars().count() as f64 * 1.6 + 4.0).clamp(10.0, 40.0);
        let _ = sheet.set_column_width(i as u16, width as f64);
    }

    wb.save_to_buffer().map_err(|e| AppError::Report {
        detail: format!("生成 xlsx 失败：{e}"),
    })
}

/// 单元格取值类型。
///
/// `Pct` 是 0–100 的利用率值（写入时除以 100 配合 `0.00%` 格式）；
/// 时间列统一以 `Text` 输出 `YYYY-MM-DD HH:MM:SS` 字符串（PRD §3 显示要求），
/// 避免依赖 rust_xlsxwriter 的日期时间转换 API。
enum CellValue {
    Pct(f64),
    Text(String),
    Na,
}

/// 从 CardRecord + 资产值取出某列对应的单元格内容。
fn cell_value(
    rec: &CardRecord,
    col: &str,
    mapping_values: &HashMap<(usize, String), String>,
    row_idx: usize,
) -> CellValue {
    // 时间戳 → "YYYY-MM-DD HH:MM:SS" 字符串
    let ts = |dt: chrono::DateTime<chrono::Utc>| dt.format("%Y-%m-%d %H:%M:%S").to_string();
    match col {
        "数据来源" => CellValue::Text(rec.source_name.clone()),
        "主机IP" => CellValue::Text(rec.host_ip.clone()),
        "节点名称" => CellValue::Text(rec.node_name.clone()),
        "计算卡编号" => CellValue::Text(rec.card_id.clone()),
        "设备类型" => CellValue::Text(rec.device_type.clone()),
        "Namespace" => CellValue::Text(rec.namespace.clone()),
        "Pod" => CellValue::Text(rec.pod.clone()),
        "容器名称" => CellValue::Text(rec.container.clone()),
        "取值时间范围" => CellValue::Text(format!(
            "{} ~ {}",
            rec.range_start.format("%Y-%m-%d %H:%M:%S"),
            rec.range_end.format("%Y-%m-%d %H:%M:%S")
        )),
        "核心利用率平均值" => rec.core_avg.map(CellValue::Pct).unwrap_or(CellValue::Na),
        "核心利用率峰值" => rec.core_peak.map(CellValue::Pct).unwrap_or(CellValue::Na),
        "核心利用率峰值出现时间" => rec
            .core_peak_time
            .map(ts)
            .map(CellValue::Text)
            .unwrap_or(CellValue::Na),
        "显存占用率平均值" => rec.mem_avg.map(CellValue::Pct).unwrap_or(CellValue::Na),
        "显存占用率峰值" => rec.mem_peak.map(CellValue::Pct).unwrap_or(CellValue::Na),
        "显存占用率峰值出现时间" => rec
            .mem_peak_time
            .map(ts)
            .map(CellValue::Text)
            .unwrap_or(CellValue::Na),
        other => {
            // 映射列：从 mapping_values 取，未命中写空串
            match mapping_values.get(&(row_idx, other.to_string())) {
                Some(v) => CellValue::Text(v.clone()),
                None => CellValue::Text(String::new()),
            }
        }
    }
}
