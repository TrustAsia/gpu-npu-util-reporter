//! 端到端渲染集成测试：构造一条 `CardRecord` + 命中阈值触发器，渲染为 xlsx
//! 字节缓冲，用 calamine 读回断言行数。
//!
//! 注：calamine 0.26 稳定 API 不暴露单元格填充色，因此"染色命中"由
//! highlight 模块的单元测试覆盖；本测试只验证渲染产出有效 xlsx 且行列数正确。

use calamine::{open_workbook_from_rs, Reader, Xlsx};
use chrono::TimeZone;
use chrono::Utc;
use gpu_npu_util_reporter::highlight::{HexColor, ThresholdTriggers, TriggerConfig};
use gpu_npu_util_reporter::mapper::BASE_COLUMNS;
use gpu_npu_util_reporter::processor::CardRecord;
use gpu_npu_util_reporter::reporter::{render_to_buffer, ReportSpec};
use std::collections::HashMap;

#[test]
fn renders_report_with_highlight_and_reads_back() {
    let rec = CardRecord {
        source_name: "prod".into(),
        host_ip: "1.1.1.1".into(),
        node_name: "node-1".into(),
        card_id: "0".into(),
        device_type: "NVIDIA A10".into(),
        namespace: "default".into(),
        pod: "p1".into(),
        container: "c1".into(),
        core_avg: Some(90.0),
        core_peak: Some(99.0),
        core_peak_time: Some(Utc.timestamp_opt(1000, 0).unwrap()),
        mem_avg: Some(20.0),
        mem_peak: Some(25.0),
        mem_peak_time: Some(Utc.timestamp_opt(1060, 0).unwrap()),
        range_start: Utc.timestamp_opt(0, 0).unwrap(),
        range_end: Utc.timestamp_opt(2000, 0).unwrap(),
    };
    let tr = ThresholdTriggers {
        core_avg_above: Some(TriggerConfig {
            enabled: true,
            threshold: 80.0,
            color: HexColor("#FF0000".into()),
        }),
        ..Default::default()
    };
    let spec = ReportSpec {
        base_columns: BASE_COLUMNS.iter().map(ToString::to_string).collect(),
        mapping_renames: vec![],
    };
    let buf = render_to_buffer(&[rec], &spec, &[], &tr, &HashMap::new()).unwrap();
    assert!(buf.len() > 1000, "应生成非空 xlsx 字节");

    // 用 calamine 读回断言行数
    let mut r: Xlsx<_> = open_workbook_from_rs(std::io::Cursor::new(buf)).unwrap();
    let name = r.sheet_names()[0].clone();
    let range = r.worksheet_range(&name).unwrap();
    assert_eq!(range.height(), 2, "1 表头 + 1 数据行");
    assert_eq!(range.width(), BASE_COLUMNS.len(), "列数应为基础列数");
}
