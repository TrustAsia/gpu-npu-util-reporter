//! 报告渲染层模块。
//!
//! 隔离 `rust_xlsxwriter`，专职把 `Vec<CardRecord>` + 列布局 + 染色决策写成带样式的
//! `.xlsx`。基础列顺序来自 [`mapper::BASE_COLUMNS`](crate::mapper::BASE_COLUMNS)，
//! 映射列位置由 mapper 计算后传入；阈值染色由 highlight 计算命中后传入颜色。

use crate::error::AppError;
use crate::highlight::{HexColor, ThresholdTriggers};
use crate::mapper::compute_column_order;
use crate::mapper::MappingColumn;
use crate::processor::CardRecord;
use chrono_tz::Tz;
use rust_xlsxwriter::{Color, Format, Workbook};
use std::collections::HashMap;
use std::hash::BuildHasher;

/// 报表列规格：基础列清单 + 映射列 rename（用于校验/未来扩展）。
pub struct ReportSpec {
    pub base_columns: Vec<String>,
    /// 映射列 rename 清单（当前未直接参与渲染，列顺序已由 `compute_column_order` 决定）。
    pub mapping_renames: Vec<String>,
}

/// 把记录写为 .xlsx 字节缓冲。
///
/// - 首行冻结 + 加粗 + 深蓝底白字
/// - 利用率列存为 value/100，数字格式 0.00%；N/A 写字符串 "N/A"
/// - 时间列以 `YYYY-MM-DD HH:MM:SS` 文本输出（PRD §3 显示要求；
///   规避 `rust_xlsxwriter` 0.79 中 chrono 不实现 `IntoExcelDateTime` 的限制）
/// - 命中染色单元格套对应 HEX 背景色 Format
///
/// # Errors
///
/// 返回 [`AppError::Report`] 当 Excel 写操作失败。
///
/// # Panics
///
/// 当 `compute_column_order` 产出的列名不在 `col_index` 中时 panic（内部逻辑一致性错误）。
#[allow(clippy::too_many_lines)]
#[allow(clippy::missing_panics_doc)]
pub fn render_to_buffer<S: BuildHasher>(
    records: &[CardRecord],
    spec: &ReportSpec,
    mapping_columns: &[MappingColumn],
    thresholds: &ThresholdTriggers,
    mapping_values: &HashMap<(usize, String), String, S>, // (行索引, rename) -> 资产值
    tz: Tz,
) -> Result<Vec<u8>, AppError> {
    // 让 mapping_renames 不触发未使用警告（保留字段以备未来校验）。
    let _ = &spec.mapping_renames;

    let base_refs: Vec<&str> = spec.base_columns.iter().map(String::as_str).collect();
    let order = compute_column_order(&base_refs, mapping_columns);

    // 构建借用的映射值索引：(行索引, 列名) → 资产值引用。
    // 避免在 cell_value 热路径中每格分配 String 做 HashMap key 查询。
    let mapping_borrowed: HashMap<(usize, &str), &str> = mapping_values
        .iter()
        .map(|((row, col), val)| ((*row, col.as_str()), val.as_str()))
        .collect();

    let mut wb = Workbook::new();
    let sheet = wb
        .add_worksheet()
        .set_name("利用率报表")
        .map_err(|e| AppError::Report {
            detail: format!("设置工作表名失败：{e}"),
        })?;

    let header_fmt = Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0x001F_4E79))
        .set_font_color(Color::White);
    let pct_fmt = Format::new().set_num_format("0.00%");
    let num_fmt = Format::new().set_num_format("0.00");

    // 列名 → 列索引
    let col_index: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i))
        .collect();

    // 写表头
    for (i, name) in order.iter().enumerate() {
        // Excel 列数上限 16384，远在 u16 范围内。
        #[allow(clippy::cast_possible_truncation)]
        let col = i as u16;
        sheet
            .write_string_with_format(0, col, name, &header_fmt)
            .map_err(|e| AppError::Report {
                detail: format!("写表头失败：{e}"),
            })?;
    }
    sheet.set_freeze_panes(1, 0).map_err(|e| AppError::Report {
        detail: format!("冻结首行失败：{e}"),
    })?;

    // 命中染色单元格的百分比 Format 构造器（按 HEX）。
    // HexColor 已在反序列化/parse 阶段校验为 #RRGGBB（大写），此处解析必然成功；
    // 若因代码 bug 导致意外值，用 match 显式回退到红色（而非静默变黑）。
    let pct_color = |hex: &HexColor| -> Format {
        let rgb = u32::from_str_radix(&hex.value()[1..], 16).unwrap_or(0xFF0000);
        Format::new()
            .set_background_color(Color::RGB(rgb))
            .set_num_format("0.00%")
    };
    let num_color = |hex: &HexColor| -> Format {
        let rgb = u32::from_str_radix(&hex.value()[1..], 16).unwrap_or(0xFF0000);
        Format::new()
            .set_background_color(Color::RGB(rgb))
            .set_num_format("0.00")
    };

    // 每列内容的显示宽度（CJK 约占 2 个拉丁字符宽），用于列宽自适应（I6 修复）。
    let mut col_max_width: Vec<f64> = vec![0.0; order.len()];

    for (row_idx, rec) in records.iter().enumerate() {
        // Excel 行数上限 1048576，远在 u32 范围内。
        #[allow(clippy::cast_possible_truncation)]
        let excel_row = (row_idx + 1) as u32;
        // 计算该行染色决策（列名 → HEX）
        let hits = thresholds.evaluate_row(rec);
        let hit_colors: HashMap<&str, &HexColor> =
            hits.iter().map(|h| (h.column, h.color)).collect();

        for name in &order {
            let idx = *col_index.get(name.as_str()).unwrap_or_else(|| {
                panic!("列「{name}」不在 col_index 中，compute_column_order 结果异常")
            });
            // Excel 列数上限 16384，远在 u16 范围内。
            #[allow(clippy::cast_possible_truncation)]
            let col = idx as u16;
            let hit_color = hit_colors.get(name.as_str()).copied();
            let v = cell_value(rec, name, &mapping_borrowed, row_idx, tz);
            // 累计该列内容的显示宽度（I6）。
            let text = match &v {
                CellValue::Pct(p) => format_pct_text(*p),
                CellValue::Number(n) => format!("{n:.2}"),
                CellValue::Count(n) => n.to_string(),
                CellValue::Text(t) => t.clone(),
                CellValue::Na => "N/A".into(),
            };
            let w = display_width(&text);
            if w > col_max_width[idx] {
                col_max_width[idx] = w;
            }
            match v {
                CellValue::Pct(p) => {
                    let fmt = hit_color.map_or_else(|| pct_fmt.clone(), pct_color);
                    sheet
                        .write_number_with_format(excel_row, col, p / 100.0, &fmt)
                        .map_err(|e| AppError::Report {
                            detail: format!("写单元格失败：{e}"),
                        })?;
                }
                CellValue::Number(n) => {
                    let fmt = hit_color.map_or_else(|| num_fmt.clone(), num_color);
                    sheet
                        .write_number_with_format(excel_row, col, n, &fmt)
                        .map_err(|e| AppError::Report {
                            detail: format!("写单元格失败：{e}"),
                        })?;
                }
                CellValue::Count(n) => {
                    #[allow(clippy::cast_precision_loss)]
                    let val = n as f64;
                    sheet
                        .write_number(excel_row, col, val)
                        .map_err(|e| AppError::Report {
                            detail: format!("写数据量失败：{e}"),
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

    // 列宽自适应：取 max(表头宽度, 内容宽度)，按 CJK 双倍宽估算，clamp [10, 50]（I6）。
    for (i, name) in order.iter().enumerate() {
        let header_w = display_width(name);
        let content_w = col_max_width.get(i).copied().unwrap_or(0.0);
        let width = header_w.max(content_w).clamp(10.0, 50.0);
        // Excel 列数上限 16384，远在 u16 范围内。
        #[allow(clippy::cast_possible_truncation)]
        sheet
            .set_column_width(i as u16, width)
            .map_err(|e| AppError::Report {
                detail: format!("设置列宽失败：{e}"),
            })?;
    }

    wb.save_to_buffer().map_err(|e| AppError::Report {
        detail: format!("生成 xlsx 失败：{e}"),
    })
}

/// 单元格取值类型。
///
/// `Pct` 是 0–100 的利用率值（写入时除以 100 配合 `0.00%` 格式）；
/// `Number` 是绝对值（如温度 °C、功率 W），以 `0.00` 格式写入；
/// 时间列统一以 `Text` 输出 `YYYY-MM-DD HH:MM:SS` 字符串（PRD §3 显示要求），
/// 避免依赖 `rust_xlsxwriter` 的日期时间转换 API。
enum CellValue {
    Pct(f64),
    /// 绝对值（如温度 °C、功率 W），以两位小数写入 Excel。
    Number(f64),
    /// 数据量（整数），以普通数字写入 Excel。
    Count(usize),
    Text(String),
    Na,
}

/// 从 `CardRecord` + 资产值取出某列对应的单元格内容。
fn cell_value(
    rec: &CardRecord,
    col: &str,
    mapping_borrowed: &HashMap<(usize, &str), &str>,
    row_idx: usize,
    tz: Tz,
) -> CellValue {
    // UTC 时间戳 → 按配置时区渲染为 "YYYY-MM-DD HH:MM:SS" 字符串
    let ts = |dt: chrono::DateTime<chrono::Utc>| {
        dt.with_timezone(&tz)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    };
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
            rec.range_start
                .with_timezone(&tz)
                .format("%Y-%m-%d %H:%M:%S"),
            rec.range_end.with_timezone(&tz).format("%Y-%m-%d %H:%M:%S")
        )),
        "核心利用率平均值" => rec.core_avg.map_or(CellValue::Na, CellValue::Pct),
        "核心利用率峰值" => rec.core_peak.map_or(CellValue::Na, CellValue::Pct),
        "核心利用率峰值出现时间" => rec
            .core_peak_time
            .map(ts)
            .map_or(CellValue::Na, CellValue::Text),
        "核心利用率数据量" => rec.core_count.map_or(CellValue::Na, CellValue::Count),
        "核心利用率首条数据时间" => rec
            .core_first_time
            .map(ts)
            .map_or(CellValue::Na, CellValue::Text),
        "核心利用率末条数据时间" => rec
            .core_last_time
            .map(ts)
            .map_or(CellValue::Na, CellValue::Text),
        "显存占用率平均值" => rec.mem_avg.map_or(CellValue::Na, CellValue::Pct),
        "显存占用率峰值" => rec.mem_peak.map_or(CellValue::Na, CellValue::Pct),
        "显存占用率峰值出现时间" => rec
            .mem_peak_time
            .map(ts)
            .map_or(CellValue::Na, CellValue::Text),
        "显存占用率数据量" => rec.mem_count.map_or(CellValue::Na, CellValue::Count),
        "显存占用率首条数据时间" => rec
            .mem_first_time
            .map(ts)
            .map_or(CellValue::Na, CellValue::Text),
        "显存占用率末条数据时间" => rec
            .mem_last_time
            .map(ts)
            .map_or(CellValue::Na, CellValue::Text),
        "设备温度平均值" => rec.temp_avg.map_or(CellValue::Na, CellValue::Number),
        "设备温度峰值" => rec.temp_peak.map_or(CellValue::Na, CellValue::Number),
        "设备温度峰值出现时间" => rec
            .temp_peak_time
            .map(ts)
            .map_or(CellValue::Na, CellValue::Text),
        "设备温度数据量" => rec.temp_count.map_or(CellValue::Na, CellValue::Count),
        "设备温度首条数据时间" => rec
            .temp_first_time
            .map(ts)
            .map_or(CellValue::Na, CellValue::Text),
        "设备温度末条数据时间" => rec
            .temp_last_time
            .map(ts)
            .map_or(CellValue::Na, CellValue::Text),
        "设备功率平均值" => rec.power_avg.map_or(CellValue::Na, CellValue::Number),
        "设备功率峰值" => rec.power_peak.map_or(CellValue::Na, CellValue::Number),
        "设备功率峰值出现时间" => rec
            .power_peak_time
            .map(ts)
            .map_or(CellValue::Na, CellValue::Text),
        "设备功率数据量" => rec.power_count.map_or(CellValue::Na, CellValue::Count),
        "设备功率首条数据时间" => rec
            .power_first_time
            .map(ts)
            .map_or(CellValue::Na, CellValue::Text),
        "设备功率末条数据时间" => rec
            .power_last_time
            .map(ts)
            .map_or(CellValue::Na, CellValue::Text),
        "主机CPU利用率平均值" => rec.host_cpu_avg.map_or(CellValue::Na, CellValue::Pct),
        "主机CPU利用率峰值" => rec.host_cpu_peak.map_or(CellValue::Na, CellValue::Pct),
        "主机CPU利用率峰值出现时间" => rec
            .host_cpu_peak_time
            .map(ts)
            .map_or(CellValue::Na, CellValue::Text),
        "主机内存利用率平均值" => rec.host_mem_avg.map_or(CellValue::Na, CellValue::Pct),
        "主机内存利用率峰值" => rec.host_mem_peak.map_or(CellValue::Na, CellValue::Pct),
        "主机内存利用率峰值出现时间" => rec
            .host_mem_peak_time
            .map(ts)
            .map_or(CellValue::Na, CellValue::Text),
        "主机句柄数平均值" => rec.host_handle_avg.map_or(CellValue::Na, CellValue::Number),
        "主机句柄数峰值" => rec
            .host_handle_peak
            .map_or(CellValue::Na, CellValue::Number),
        "主机句柄数峰值出现时间" => rec
            .host_handle_peak_time
            .map(ts)
            .map_or(CellValue::Na, CellValue::Text),
        other => {
            // 映射列：从 mapping_borrowed 取，未命中写空串
            mapping_borrowed.get(&(row_idx, other)).map_or_else(
                || CellValue::Text(String::new()),
                |v| CellValue::Text((*v).to_string()),
            )
        }
    }
}

/// 估算字符串在 Excel 列里的显示宽度（用于 I6 列宽自适应）。
///
/// 规则：CJK 字符（含全角标点）按 2 个单位计，其余按 1 个单位；末尾留 ~2 单位
/// padding。例如 "192.168.100.200"（15 半角）→ 17；"核心利用率峰值"（7 CJK）→ 16。
pub(crate) fn display_width(s: &str) -> f64 {
    let mut w = 0.0;
    for c in s.chars() {
        if is_wide(c) {
            w += 2.0;
        } else {
            w += 1.0;
        }
    }
    w + 2.0 // padding
}

/// 判断字符是否按"宽"（≈2 个拉丁字符宽）估算。
const fn is_wide(c: char) -> bool {
    let cp = c as u32;
    // CJK 统一表意、CJK 标点、全角 ASCII、平假名/片假名/谚文等常见区间。
    matches!(cp,
        0x1100..=0x115F |     // 谚文 Jamo
        0x2E80..=0x303E |     // CJK 部首/标点
        0x3041..=0x33FF |     // 平假名/片假名/CJK 符号
        0x3400..=0x4DBF |     // CJK 扩展 A
        0x4E00..=0x9FFF |     // CJK 统一表意
        0xA000..=0xA4CF |     // 彝文
        0xAC00..=0xD7A3 |     // 谚文音节
        0xF900..=0xFAFF |     // CJK 兼容表意
        0xFE30..=0xFE4F |     // CJK 兼容形式
        0xFF00..=0xFF60 |     // 全角 ASCII
        0xFFE0..=0xFFE6       // 全角符号
    )
}

/// 把 0–100 的利用率值格式化为报表里会显示的文本（用于估算列宽，与 0.00% 格式一致）。
fn format_pct_text(v: f64) -> String {
    format!("{v:.2}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_width_counts_cjk_as_double() {
        // 7 个 CJK → 14 + 2 padding = 16
        assert!((display_width("核心利用率峰值") - 16.0).abs() < 1e-9);
        // 15 个半角 → 15 + 2 = 17
        assert!((display_width("192.168.100.200") - 17.0).abs() < 1e-9);
        // 时间戳 "2026-06-18 00:00:00" 19 半角 → 21
        assert!((display_width("2026-06-18 00:00:00") - 21.0).abs() < 1e-9);
    }

    #[test]
    fn display_width_handles_mixed_and_empty() {
        // "主机IP" = 2 CJK + 2 ASCII = 6 + 2 padding = 8
        assert!((display_width("主机IP") - 8.0).abs() < 1e-9);
        assert!((display_width("") - 2.0).abs() < 1e-9);
    }

    #[test]
    fn format_pct_text_matches_excel_format() {
        assert_eq!(format_pct_text(90.0), "90.00%");
        assert_eq!(format_pct_text(22.5), "22.50%");
    }
}
