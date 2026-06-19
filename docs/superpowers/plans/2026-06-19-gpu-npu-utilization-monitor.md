# GPU/NPU 多源利用率监控与报表生成系统 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 构建一个 Rust CLI 工具，从多个 Prometheus 源提取 GPU/NPU 在指定时间范围内的利用率与显存占用，聚合后输出带样式（含阈值染色）的 Excel 报表。

**Architecture:** 六个高内聚低耦合模块，单向数据流 `config → fetcher → processor → mapper → highlight → reporter`，trait 边界（`MetricFetcher`）解耦数据源、数据化设备规则（`DeviceSpec`）。单点失败降级为 N/A，全程不 panic。

**Tech Stack:** Rust 2021 / tokio + reqwest（异步 HTTP）/ serde + serde_yaml（配置）/ clap（CLI）/ chrono（时间）/ csv + calamine（资产表）/ rust_xlsxwriter（报表）/ thiserror（错误）。已验证依赖版本在 Rust 1.94 下编译通过。

**关键约束（贯穿所有任务）：**
- 所有 `struct`/`trait`/复杂函数必须带**中文 doc 注释**。
- **禁用** `unwrap()`/`expect()`/panic；全部走 `Result` + `?`。
- 致命错误用中文 `[错误]...` 提示并退出码 1；非致命用 `[警告]...`、对应单元格 N/A、退出码 0。
- 测试先行（TDD）：每个任务先写失败测试，再实现，再验证通过。

---

## 文件结构

| 文件 | 职责 |
|------|------|
| `Cargo.toml` | 依赖声明 |
| `src/error.rs` | 统一错误类型 `AppError`（thiserror） |
| `src/devices.rs` | `DeviceSpec` / `MemoryStrategy` / `LabelMapping` 及 NVIDIA A10、Ascend 910B 预设 |
| `src/highlight.rs` | `ThresholdTriggers` / `TriggerConfig` / `HexColor` / `evaluate_row` |
| `src/processor.rs` | `CardRecord` / 聚合算法 / HBM fallback / 归属取值 |
| `src/mapper.rs` | `MappingConfig` / 资产表加载 / Join / 列位置排布 |
| `src/fetcher.rs` | `MetricFetcher` trait / `Series` / `PrometheusFetcher` |
| `src/config.rs` | `AppConfig` 及子结构 / YAML 解析 / 默认配置生成 / CLI 合并 |
| `src/reporter.rs` | Excel 渲染（列布局、样式、染色集成） |
| `src/main.rs` | CLI 入口与编排 |

**实现顺序**：先纯数据/逻辑层（无 IO，最易测）→ config → fetcher → 编排 → reporter。每个任务产出可独立编译、可测的增量。

---

## Task 1: 项目脚手架与依赖

**Files:**
- Create: `Cargo.toml`
- Create: `src/main.rs`（占位）

- [ ] **Step 1: 创建 `Cargo.toml`**

```toml
[package]
name = "gpu-util-monitor"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { version = "1", features = ["full"] }
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
chrono = { version = "0.4", features = ["serde"] }
thiserror = "1"
anyhow = "1"
csv = "1"
calamine = "0.26"
rust_xlsxwriter = "0.79"
futures = "0.3"
async-trait = "0.1"
```

- [ ] **Step 2: 创建占位 `src/main.rs`**

```rust
/// 程序入口（后续任务填充）。
fn main() {
    println!("gpu-util-monitor: 脚手架就绪");
}
```

- [ ] **Step 3: 验证编译**

Run: `cargo build`
Expected: `Finished` 无错误（首次会编译大量依赖，耗时较长）。

- [ ] **Step 4: 提交**

```bash
git add Cargo.toml Cargo.lock src/main.rs
git commit -m "chore: 初始化 Rust 项目脚手架与依赖"
```

---

## Task 2: error 模块（统一错误类型）

**Files:**
- Create: `src/error.rs`
- Modify: `src/main.rs`（声明 `mod error;`）

- [ ] **Step 1: 写失败测试 `src/error.rs`**

整个 `error.rs` 同时是实现和被测对象。先写实现骨架与一个测试：

```rust
//! 统一错误类型模块。
//!
//! 全程序使用 [`AppError`] 作为错误载体，配合 `thiserror` 派生
//! `Error`/`Display`，保证所有错误都带中文、对人类友好的上下文。
//! 非致命情况（单卡/单源失败）用 [`AppError::Warning`] 表达，不中断流程。

use thiserror::Error;

/// 应用统一错误类型。
///
/// 设计意图：把所有可预见的失败场景枚举化，每个变体携带足够定位的字段，
/// 其 `Display` 输出即面向终端用户的中文提示。
#[derive(Error, Debug)]
pub enum AppError {
    /// 配置文件解析或字段缺失等致命错误。
    #[error("[错误] 配置文件 {path} 解析失败：{reason}")]
    Config { path: String, reason: String },

    /// 无法连接到 Prometheus（网络层）。
    #[error("[错误] 无法连接到 Prometheus 数据源 {source}（{url}），请检查网络或配置：{detail}")]
    Prometheus { source: String, url: String, detail: String },

    /// PromQL 查询被 Prometheus 拒绝或返回非成功状态。
    #[error("[错误] PromQL 查询返回异常（{source}）：{detail}")]
    Promql { source: String, detail: String },

    /// 时间字符串不符合 `YYYY-MM-DD HH:MM:SS`。
    #[error("[错误] 时间格式无效：{raw}，请使用 YYYY-MM-DD HH:MM:SS")]
    TimeFormat { raw: String },

    /// 阈值染色颜色不是合法 HEX。
    #[error("[错误] 阈值触发器 {trigger} 的颜色 {raw} 不是合法的 HEX 颜色（需为 #RRGGBB 或 #RGB）")]
    InvalidColor { trigger: String, raw: String },

    /// 资产表加载或解析失败。
    #[error("[错误] 资产表加载失败（{path}）：{detail}")]
    Mapping { path: String, detail: String },

    /// 报表写入失败（磁盘、权限等）。
    #[error("[错误] 报表写入失败：{detail}")]
    Report { detail: String },

    /// 非致命警告：仅记录、不中断。
    #[error("[警告] {msg}")]
    Warning { msg: String },
}

impl AppError {
    /// 判断是否为非致命警告（调用方据此决定是否继续）。
    pub fn is_warning(&self) -> bool {
        matches!(self, AppError::Warning { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_error_displays_chinese() {
        let e = AppError::Config {
            path: "./config.yaml".into(),
            reason: "time_range.start 字段缺失".into(),
        };
        let s = format!("{e}");
        assert!(s.contains("[错误]"));
        assert!(s.contains("./config.yaml"));
        assert!(s.contains("time_range.start"));
    }

    #[test]
    fn warning_is_non_fatal() {
        let e = AppError::Warning { msg: "卡片无数据".into() };
        assert!(e.is_warning());
    }

    #[test]
    fn fatal_errors_are_not_warnings() {
        let e = AppError::TimeFormat { raw: "x".into() };
        assert!(!e.is_warning());
    }
}
```

- [ ] **Step 2: 在 `src/main.rs` 顶部声明模块**

把 `src/main.rs` 改为：

```rust
/// 程序入口（后续任务填充）。
mod error;

fn main() {
    println!("gpu-util-monitor: 脚手架就绪");
}
```

- [ ] **Step 3: 运行测试验证通过**

Run: `cargo test error`
Expected: 3 个测试 PASS。

- [ ] **Step 4: 提交**

```bash
git add src/error.rs src/main.rs
git commit -m "feat(error): 统一错误类型 AppError 与中文提示"
```

---

## Task 3: devices 模块（设备指标配方）

**Files:**
- Create: `src/devices.rs`
- Modify: `src/main.rs`（加 `mod devices;`）

- [ ] **Step 1: 写实现与测试 `src/devices.rs`**

```rust
//! 设备类型"指标配方"模块。
//!
//! 把 PRD §2.2 中 NVIDIA A10 与 Ascend 910B 的指标抽取规则抽象成数据
//! （[`DeviceSpec`]），让 fetcher/processor 据此决定查什么、怎么算，
//! 而不是把设备特定逻辑写死在代码分支里。新增设备类型只需新增一份配方。

use serde::{Deserialize, Serialize};

/// 标签名映射：把统一的逻辑归属字段映射到各 exporter 实际使用的标签名。
///
/// 例如 NPU 用 `container_name`/`pod_name`，DCGM 常用 `container`/`pod`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LabelMapping {
    /// 容器标签名。
    pub container: String,
    /// Pod 标签名。
    pub pod: String,
    /// Namespace 标签名。
    pub namespace: String,
}

/// 显存占用率的计算策略。
///
/// 三个变体分别对应三种来源：直接读现成利用率指标、用 used/(used+free) 组合、
/// 用 used/total 组合。processor 据 variant 决定聚合方式。
///
/// serde 表示采用 `#[serde(untagged)]` + newtype 包装，使 YAML 形如
/// `composite_ratio: { used, free }` / `direct_metric: { metric, fallback }` /
/// `composite_from_total: { used, total }`（外部命名的单键 map），兼容 serde_yaml
/// 对带字段变体的限制（serde_yaml 不支持默认的 externally-tagged 字段变体）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum MemoryStrategy {
    /// used/(used+free)*100 组合（如 GPU 的 FB_USED/(FB_USED+FB_FREE)）。
    CompositeRatio(CompositeRatioBody),
    /// 直接读一个利用率指标（如 NPU 的 `npu_chip_info_hbm_utilization`）。
    /// `fallback` 在该指标查询为空时启用。
    DirectMetric(DirectMetricBody),
    /// used/total*100 组合（如 NPU fallback 的 hbm_used/hbm_total）。
    CompositeFromTotal(CompositeFromTotalBody),
}

/// `composite_ratio` 包装体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompositeRatioBody {
    pub composite_ratio: UsedFree,
}
/// `composite_from_total` 包装体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompositeFromTotalBody {
    pub composite_from_total: UsedTotal,
}
/// `direct_metric` 包装体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DirectMetricBody {
    pub direct_metric: DirectInner,
}
/// used/free 两个字段。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsedFree {
    pub used: String,
    pub free: String,
}
/// used/total 两个字段。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsedTotal {
    pub used: String,
    pub total: String,
}
/// direct 指标 + 可选 fallback。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DirectInner {
    pub metric: String,
    #[serde(default)]
    pub fallback: Option<Box<MemoryStrategy>>,
}

impl MemoryStrategy {
    /// 便捷构造：GPU 组合公式。
    pub fn composite_ratio(used: &str, free: &str) -> Self {
        MemoryStrategy::CompositeRatio(CompositeRatioBody {
            composite_ratio: UsedFree { used: used.into(), free: free.into() },
        })
    }
    /// 便捷构造：NPU used/total 组合。
    pub fn composite_from_total(used: &str, total: &str) -> Self {
        MemoryStrategy::CompositeFromTotal(CompositeFromTotalBody {
            composite_from_total: UsedTotal { used: used.into(), total: total.into() },
        })
    }
    /// 便捷构造：direct 指标 + 可选 fallback。
    pub fn direct(metric: &str, fallback: Option<MemoryStrategy>) -> Self {
        MemoryStrategy::DirectMetric(DirectMetricBody {
            direct_metric: DirectInner {
                metric: metric.into(),
                fallback: fallback.map(Box::new),
            },
        })
    }
}

/// 一个设备类型的完整"指标配方"。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceSpec {
    /// 报表"设备类型"列显示名，如 "NVIDIA A10"。
    pub display_name: String,
    /// 核心利用率指标名。
    pub core_util_metric: String,
    /// 显存占用率计算策略。
    pub memory: MemoryStrategy,
    /// 卡编号所在标签名（如 GPU 的 `gpu`、NPU 的 `id`）。
    pub card_id_label: String,
    /// 归属标签映射。
    pub labels: LabelMapping,
}

/// NVIDIA A10 预设配方（基于 DCGM Exporter）。
pub fn nvidia_a10_spec() -> DeviceSpec {
    DeviceSpec {
        display_name: "NVIDIA A10".into(),
        core_util_metric: "DCGM_FI_DEV_GPU_UTIL".into(),
        memory: MemoryStrategy::composite_ratio("DCGM_FI_DEV_FB_USED", "DCGM_FI_DEV_FB_FREE"),
        card_id_label: "gpu".into(),
        labels: LabelMapping {
            container: "container".into(),
            pod: "pod".into(),
            namespace: "namespace".into(),
        },
    }
}

/// Ascend 910B 预设配方（基于 NPU Exporter）。
///
/// 显存优先读 `npu_chip_info_hbm_utilization`；为空时 fallback 到
/// `hbm_used_memory / hbm_total_memory`（PRD §2.2）。
pub fn ascend_910b_spec() -> DeviceSpec {
    DeviceSpec {
        display_name: "Ascend 910B".into(),
        core_util_metric: "npu_chip_info_utilization".into(),
        memory: MemoryStrategy::direct(
            "npu_chip_info_hbm_utilization",
            Some(MemoryStrategy::composite_from_total(
                "npu_chip_info_hbm_used_memory",
                "npu_chip_info_hbm_total_memory",
            )),
        ),
        card_id_label: "id".into(),
        labels: LabelMapping {
            container: "container_name".into(),
            pod: "pod_name".into(),
            namespace: "namespace".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_a10_spec_uses_composite_ratio() {
        let s = nvidia_a10_spec();
        assert_eq!(s.core_util_metric, "DCGM_FI_DEV_GPU_UTIL");
        assert_eq!(s.card_id_label, "gpu");
        match &s.memory {
            MemoryStrategy::CompositeRatio(b) => {
                assert_eq!(b.composite_ratio.used, "DCGM_FI_DEV_FB_USED");
                assert_eq!(b.composite_ratio.free, "DCGM_FI_DEV_FB_FREE");
            }
            other => panic!("期望 CompositeRatio，得到 {:?}", other),
        }
    }

    #[test]
    fn ascend_910b_spec_has_fallback_chain() {
        let s = ascend_910b_spec();
        assert_eq!(s.card_id_label, "id");
        match &s.memory {
            MemoryStrategy::DirectMetric(b) => {
                assert_eq!(b.direct_metric.metric, "npu_chip_info_hbm_utilization");
                let fb = b.direct_metric.fallback.as_ref().expect("应有 fallback");
                assert!(matches!(fb.as_ref(), MemoryStrategy::CompositeFromTotal(_)));
            }
            other => panic!("期望 DirectMetric，得到 {:?}", other),
        }
    }

    #[test]
    fn device_spec_round_trips_through_yaml() {
        let s = ascend_910b_spec();
        let yaml = serde_yaml::to_string(&s).unwrap();
        let back: DeviceSpec = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(s, back);
    }
}
```

**`MemoryStrategy` 的 YAML 表示**（serde untagged + newtype 包装）。对应配置示例：

```yaml
memory:
  composite_ratio: { used: "DCGM_FI_DEV_FB_USED", free: "DCGM_FI_DEV_FB_FREE" }
# 或
memory:
  direct_metric:
    metric: "npu_chip_info_hbm_utilization"
    fallback:
      composite_from_total: { used: "npu_chip_info_hbm_used_memory", total: "npu_chip_info_hbm_total_memory" }
```

> 设计原因：serde_yaml 不支持 serde 默认的 externally-tagged 字段变体（会报 `expected a YAML tag starting with '!'`），也不支持 internally-tagged 的 struct 变体（`#[serde(tag=...)]` 对带字段变体报类型不匹配）。`#[serde(untagged)]` + newtype 包装体是带字段变体在 YAML 下唯一干净可行的方案。

- [ ] **Step 2: `src/main.rs` 加 `mod devices;`**

```rust
/// 程序入口（后续任务填充）。
mod devices;
mod error;

fn main() {
    println!("gpu-util-monitor: 脚手架就绪");
}
```

- [ ] **Step 3: 运行测试验证通过**

Run: `cargo test devices`
Expected: 3 个测试 PASS。

- [ ] **Step 4: 提交**

```bash
git add src/devices.rs src/main.rs
git commit -m "feat(devices): 设备指标配方 DeviceSpec 与 A10/910B 预设"
```

---

## Task 4: processor 模块（聚合 + CardRecord）

**Files:**
- Create: `src/processor.rs`
- Modify: `src/main.rs`（加 `mod processor;`）

- [ ] **Step 1: 写实现 `src/processor.rs`（先不含归属/HBM，聚焦聚合）**

```rust
//! 数据处理与聚合模块。
//!
//! 把 fetcher 拉回的时序点（`Series`）聚合成一张卡在时间范围内的统计结果
//! （[`CardRecord`]）：均值、峰值、峰值时间。空序列返回 `None`（报表显示 N/A）。
//! HBM fallback 与归属取值的逻辑也落在这里。

use chrono::{DateTime, Utc};

/// 一张卡的时间范围内统计结果——对应报表一行。
///
/// 所有数值字段为 `Option`：`None` 表示该卡/该指标无有效数据，报表显示 N/A。
#[derive(Debug, Clone, PartialEq)]
pub struct CardRecord {
    /// 数据来源（Prometheus 别名）。
    pub source_name: String,
    /// 主机 IP。
    pub host_ip: String,
    /// 节点名称（标签，可能为空字符串）。
    pub node_name: String,
    /// 计算卡编号。
    pub card_id: String,
    /// 设备类型显示名。
    pub device_type: String,
    /// Namespace 归属。
    pub namespace: String,
    /// Pod 归属。
    pub pod: String,
    /// 容器归属。
    pub container: String,
    /// 核心利用率平均值（0–100）。None = N/A。
    pub core_avg: Option<f64>,
    /// 核心利用率峰值。
    pub core_peak: Option<f64>,
    /// 核心峰值出现时间。
    pub core_peak_time: Option<DateTime<Utc>>,
    /// 显存占用率平均值。
    pub mem_avg: Option<f64>,
    /// 显存占用率峰值。
    pub mem_peak: Option<f64>,
    /// 显存峰值出现时间。
    pub mem_peak_time: Option<DateTime<Utc>>,
    /// 取值时间范围起点。
    pub range_start: DateTime<Utc>,
    /// 取值时间范围终点。
    pub range_end: DateTime<Utc>,
}

/// 一个带标签的时序序列（由 fetcher 产出）。
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    /// Prometheus 标签集合。
    pub labels: std::collections::HashMap<String, String>,
    /// (时间戳, 数值) 时间序列点。
    pub points: Vec<(DateTime<Utc>, f64)>,
}

/// 单指标的聚合统计。
#[derive(Debug, Clone, PartialEq)]
pub struct MetricStats {
    /// 算术平均。
    pub avg: f64,
    /// 峰值。
    pub peak: f64,
    /// 峰值出现时间。
    pub peak_time: DateTime<Utc>,
}

/// 对一组点做均值/峰值/峰值时间聚合。
///
/// 空输入返回 `None`。峰值取最大值；多个点同为最大时取最早的时间戳（稳定）。
pub fn aggregate(points: &[(DateTime<Utc>, f64)]) -> Option<MetricStats> {
    if points.is_empty() {
        return None;
    }
    let sum: f64 = points.iter().map(|(_, v)| *v).sum();
    let avg = sum / points.len() as f64;
    // 取最大值；并列取最早时间戳
    let (peak_time, peak) = points
        .iter()
        .copied()
        .max_by(|(ta, va), (tb, vb)| {
            va.partial_cmp(vb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(tb.cmp(ta))
        })
        .expect("非空序列必有最大值");
    Some(MetricStats { avg, peak, peak_time })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).unwrap()
    }

    #[test]
    fn aggregate_empty_returns_none() {
        assert!(aggregate(&[]).is_none());
    }

    #[test]
    fn aggregate_computes_avg_peak_peaktime() {
        let pts = vec![(t(0), 10.0), (t(60), 40.0), (t(120), 70.0)];
        let s = aggregate(&pts).unwrap();
        assert!((s.avg - 40.0).abs() < 1e-9);
        assert!((s.peak - 70.0).abs() < 1e-9);
        assert_eq!(s.peak_time, t(120));
    }

    #[test]
    fn aggregate_tie_picks_earliest_timestamp() {
        let pts = vec![(t(60), 50.0), (t(0), 50.0)];
        let s = aggregate(&pts).unwrap();
        assert_eq!(s.peak_time, t(0), "并列峰值应取最早时间戳");
    }
}
```

- [ ] **Step 2: `src/main.rs` 加 `mod processor;`**

```rust
/// 程序入口（后续任务填充）。
mod devices;
mod error;
mod processor;

fn main() {
    println!("gpu-util-monitor: 脚手架就绪");
}
```

- [ ] **Step 3: 运行测试验证通过**

Run: `cargo test processor`
Expected: 3 个测试 PASS。

- [ ] **Step 4: 提交**

```bash
git add src/processor.rs src/main.rs
git commit -m "feat(processor): 时序聚合 aggregate 与 CardRecord 结构"
```

---

## Task 5: processor 补充 —— HBM fallback 与归属取值

**Files:**
- Modify: `src/processor.rs`（追加函数与测试）

- [ ] **Step 1: 追加 HBM fallback 函数**

在 `src/processor.rs` 末尾（`#[cfg(test)]` 之前）追加：

```rust
/// HBM fallback：当直接利用率指标为空时，用 used/total*100 重算显存占用率序列。
///
/// `direct` 为直接利用率序列；`used`/`total` 为显存字节/MB 的原始序列。
/// 返回 fallback 后的 `Series`（点数与 used 对齐；total 为 0 的点丢弃）。
/// 调用方应：先尝试 aggregate(direct.points)；为空时再调用本函数并 aggregate 结果。
pub fn hbm_fallback_series(
    used: &Series,
    total: &Series,
) -> Series {
    // 按 timestamp 对齐 used 与 total
    let total_map: std::collections::HashMap<i64, f64> = total
        .points
        .iter()
        .map(|(ts, v)| (ts.timestamp(), *v))
        .collect();
    let mut points = Vec::new();
    for (ts, u) in &used.points {
        if let Some(tot) = total_map.get(&ts.timestamp()) {
            if *tot > 0.0 {
                points.push((*ts, u / tot * 100.0));
            }
        }
    }
    Series {
        labels: used.labels.clone(),
        points,
    }
}

/// 归属取值模式（PRD §2.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipMode {
    /// 瞬时值：查询时刻的标签。
    Instant,
    /// 末态值：时间范围内最后一个非空标签。
    LastInRange,
}

/// 从一组归属时序点中取"末态"标签值（最后一个非空字符串）。
///
/// `tagged_points` 是 (时间戳, 该标签值) 序列；空或全空返回空串。
pub fn last_non_empty(tagged_points: &[(DateTime<Utc>, String)]) -> String {
    tagged_points
        .iter()
        .rev()
        .find(|(_, v)| !v.is_empty())
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}
```

- [ ] **Step 2: 追加测试**

在 `src/processor.rs` 的 `mod tests` 内追加：

```rust
    #[test]
    fn hbm_fallback_divides_used_by_total() {
        let used = Series {
            labels: Default::default(),
            points: vec![(t(0), 50.0), (t(60), 60.0)],
        };
        let total = Series {
            labels: Default::default(),
            points: vec![(t(0), 200.0), (t(60), 0.0)], // t60 total=0 应被丢弃
        };
        let fb = hbm_fallback_series(&used, &total);
        assert_eq!(fb.points.len(), 1);
        assert!((fb.points[0].1 - 25.0).abs() < 1e-9); // 50/200*100
    }

    #[test]
    fn last_non_empty_picks_latest_nonempty() {
        let pts = vec![
            (t(0), "pod-a".to_string()),
            (t(60), "".to_string()),
            (t(120), "pod-b".to_string()),
        ];
        assert_eq!(last_non_empty(&pts), "pod-b");
    }

    #[test]
    fn last_non_empty_all_empty_returns_empty() {
        let pts = vec![(t(0), "".to_string()), (t(60), "".to_string())];
        assert_eq!(last_non_empty(&pts), "");
    }
```

- [ ] **Step 3: 运行测试验证通过**

Run: `cargo test processor`
Expected: 原 3 + 新 3 = 6 个测试 PASS。

- [ ] **Step 4: 提交**

```bash
git add src/processor.rs
git commit -m "feat(processor): HBM fallback 与归属末态取值"
```

---

## Task 6: highlight 模块（阈值染色规则）

**Files:**
- Create: `src/highlight.rs`
- Modify: `src/main.rs`（加 `mod highlight;`）

- [ ] **Step 1: 写实现与测试 `src/highlight.rs`**

```rust
//! 阈值染色规则模块（PRD §2.6）。
//!
//! 定义 8 个触发器（核心/显存 × 均值/峰值 × 高于/低于），给定一行
//! [`CardRecord`] 计算命中的"报表列名 → 颜色"映射。本模块只产出染色决策，
//! 不触碰 Excel——渲染由 reporter 消费，从而规则演进不影响渲染层。

use crate::error::AppError;
use crate::processor::CardRecord;
use serde::{Deserialize, Serialize};

/// 报表列名常量（与 reporter 的基础列保持一致）。
pub const COL_CORE_AVG: &str = "核心利用率平均值";
pub const COL_CORE_PEAK: &str = "核心利用率峰值";
pub const COL_MEM_AVG: &str = "显存占用率平均值";
pub const COL_MEM_PEAK: &str = "显存占用率峰值";

/// HEX 颜色包装类型，反序列化时校验合法性（`#RRGGBB` 或 `#RGB`）。
#[derive(Debug, Clone, PartialEq)]
pub struct HexColor(pub String);

impl HexColor {
    /// 校验并构造；非法返回 `AppError::InvalidColor`。
    /// trigger 参数仅用于错误提示上下文。
    pub fn parse(raw: &str, trigger: &str) -> Result<Self, AppError> {
        let s = raw.trim();
        let valid = if let Some(hex) = s.strip_prefix('#') {
            hex.len() == 6 || hex.len() == 3
        } else {
            false
        } && s[1..].chars().all(|c| c.is_ascii_hexdigit());
        if valid {
            Ok(HexColor(s.to_uppercase()))
        } else {
            Err(AppError::InvalidColor {
                trigger: trigger.into(),
                raw: raw.into(),
            })
        }
    }
}

impl<'de> Deserialize<'de> for HexColor {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        HexColor::parse(&s, "<配置>").map_err(serde::de::Error::custom)
    }
}

/// 单个触发器配置。`enabled: false` 则整体跳过。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TriggerConfig {
    pub enabled: bool,
    /// 0–100 的阈值。
    pub threshold: f64,
    pub color: HexColor,
}

/// 8 个触发器的显式集合；`None` 字段 = 该触发器未配置/关闭。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ThresholdTriggers {
    #[serde(default)]
    pub core_avg_above: Option<TriggerConfig>,
    #[serde(default)]
    pub core_avg_below: Option<TriggerConfig>,
    #[serde(default)]
    pub core_peak_above: Option<TriggerConfig>,
    #[serde(default)]
    pub core_peak_below: Option<TriggerConfig>,
    #[serde(default)]
    pub mem_avg_above: Option<TriggerConfig>,
    #[serde(default)]
    pub mem_avg_below: Option<TriggerConfig>,
    #[serde(default)]
    pub mem_peak_above: Option<TriggerConfig>,
    #[serde(default)]
    pub mem_peak_below: Option<TriggerConfig>,
}

/// 一条命中结果：列名 + 颜色（借用，避免克隆）。
pub struct Hit<'a> {
    pub column: &'a str,
    pub color: &'a HexColor,
}

impl ThresholdTriggers {
    /// 评估一行记录，返回命中的染色列表。
    ///
    /// 顺序遵循字段声明顺序；同一列若被多个触发器命中，取**首个**命中
    /// （由调用方按列去重时保留首次）。None/enabled:false/字段为 None 均跳过。
    /// 比较为严格 `>`/`<`（不含等于）。
    pub fn evaluate_row<'a>(&'a self, r: &CardRecord) -> Vec<Hit<'a>> {
        let mut hits = Vec::new();
        // 每列用一个闭包链：先取该列首个命中触发器
        if let Some(h) = first_hit(
            &self.core_avg_above,
            &self.core_avg_below,
            r.core_avg,
            COL_CORE_AVG,
        ) {
            hits.push(h);
        }
        if let Some(h) = first_hit(
            &self.core_peak_above,
            &self.core_peak_below,
            r.core_peak,
            COL_CORE_PEAK,
        ) {
            hits.push(h);
        }
        if let Some(h) = first_hit(
            &self.mem_avg_above,
            &self.mem_avg_below,
            r.mem_avg,
            COL_MEM_AVG,
        ) {
            hits.push(h);
        }
        if let Some(h) = first_hit(
            &self.mem_peak_above,
            &self.mem_peak_below,
            r.mem_peak,
            COL_MEM_PEAK,
        ) {
            hits.push(h);
        }
        hits
    }
}

/// 对单个列：依次尝试 above / below，返回首个命中的 Hit。
/// above 优先（字段声明顺序在前）。
fn first_hit<'a>(
    above: &'a Option<TriggerConfig>,
    below: &'a Option<TriggerConfig>,
    value: Option<f64>,
    column: &'a str,
) -> Option<Hit<'a>> {
    let v = value?;
    if let Some(t) = above {
        if t.enabled && v > t.threshold {
            return Some(Hit { column, color: &t.color });
        }
    }
    if let Some(t) = below {
        if t.enabled && v < t.threshold {
            return Some(Hit { column, color: &t.color });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processor::CardRecord;
    use chrono::TimeZone;
    use chrono::Utc;

    fn empty_record() -> CardRecord {
        CardRecord {
            source_name: "s".into(),
            host_ip: "1.1.1.1".into(),
            node_name: "".into(),
            card_id: "0".into(),
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

    fn trig(enabled: bool, threshold: f64, color: &str) -> TriggerConfig {
        TriggerConfig {
            enabled,
            threshold,
            color: HexColor::parse(color, "test").unwrap(),
        }
    }

    #[test]
    fn hexcolor_accepts_rrggbb_and_rgb() {
        assert!(HexColor::parse("#FF0000", "t").is_ok());
        assert!(HexColor::parse("#F00", "t").is_ok());
        assert!(HexColor::parse("#ff00aa", "t").is_ok());
    }

    #[test]
    fn hexcolor_rejects_invalid() {
        assert!(HexColor::parse("red", "t").is_err());
        assert!(HexColor::parse("#GGG", "t").is_err());
        assert!(HexColor::parse("#12345", "t").is_err());
        assert!(HexColor::parse("FF0000", "t").is_err()); // 缺 #
    }

    #[test]
    fn above_trigger_hits_when_value_greater() {
        let mut r = empty_record();
        r.core_avg = Some(85.0);
        let mut tr = ThresholdTriggers::default();
        tr.core_avg_above = Some(trig(true, 80.0, "#FF0000"));
        let hits = tr.evaluate_row(&r);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].column, COL_CORE_AVG);
        assert_eq!(hits[0].color.0, "#FF0000");
    }

    #[test]
    fn above_does_not_hit_at_boundary_equal() {
        let mut r = empty_record();
        r.core_avg = Some(80.0); // 等于阈值，严格 > 不命中
        let mut tr = ThresholdTriggers::default();
        tr.core_avg_above = Some(trig(true, 80.0, "#FF0000"));
        assert!(tr.evaluate_row(&r).is_empty());
    }

    #[test]
    fn below_trigger_hits_when_value_lower() {
        let mut r = empty_record();
        r.mem_peak = Some(3.0);
        let mut tr = ThresholdTriggers::default();
        tr.mem_peak_below = Some(trig(true, 5.0, "#FFA500"));
        let hits = tr.evaluate_row(&r);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].column, COL_MEM_PEAK);
    }

    #[test]
    fn disabled_trigger_is_skipped() {
        let mut r = empty_record();
        r.core_avg = Some(99.0);
        let mut tr = ThresholdTriggers::default();
        tr.core_avg_above = Some(trig(false, 80.0, "#FF0000")); // 关闭
        assert!(tr.evaluate_row(&r).is_empty());
    }

    #[test]
    fn none_field_is_skipped() {
        let r = empty_record(); // core_avg = None
        let mut tr = ThresholdTriggers::default();
        tr.core_avg_above = Some(trig(true, 80.0, "#FF0000"));
        assert!(tr.evaluate_row(&r).is_empty());
    }

    #[test]
    fn same_column_above_takes_precedence_over_below() {
        // above 与 below 同列都配且都命中，取 above（字段顺序在前）
        let mut r = empty_record();
        r.core_avg = Some(50.0);
        let mut tr = ThresholdTriggers::default();
        tr.core_avg_above = Some(trig(true, 40.0, "#FF0000"));
        tr.core_avg_below = Some(trig(true, 60.0, "#FFA500"));
        let hits = tr.evaluate_row(&r);
        assert_eq!(hits.len(), 1, "同列只产生一个命中");
        assert_eq!(hits[0].color.0, "#FF0000");
    }

    #[test]
    fn multiple_columns_each_at_most_one_hit() {
        let mut r = empty_record();
        r.core_avg = Some(90.0);
        r.mem_avg = Some(2.0);
        let mut tr = ThresholdTriggers::default();
        tr.core_avg_above = Some(trig(true, 80.0, "#FF0000"));
        tr.mem_avg_below = Some(trig(true, 10.0, "#FFA500"));
        let hits = tr.evaluate_row(&r);
        assert_eq!(hits.len(), 2);
    }
}
```

- [ ] **Step 2: `src/main.rs` 加 `mod highlight;`**

```rust
/// 程序入口（后续任务填充）。
mod devices;
mod error;
mod highlight;
mod processor;

fn main() {
    println!("gpu-util-monitor: 脚手架就绪");
}
```

- [ ] **Step 3: 运行测试验证通过**

Run: `cargo test highlight`
Expected: 9 个测试 PASS。

- [ ] **Step 4: 提交**

```bash
git add src/highlight.rs src/main.rs
git commit -m "feat(highlight): 8 触发器阈值染色规则与 HEX 校验"
```

---

## Task 7: mapper 模块（资产映射与列排布）

**Files:**
- Create: `src/mapper.rs`
- Modify: `src/main.rs`（加 `mod mapper;`）

- [ ] **Step 1: 写实现 `src/mapper.rs`（一次性、可直接编译的完整版本）**

```rust
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
///
/// serde 表示为对象 `{ direction: before|after, anchor: <列名> }`，而非外部标记枚举
/// ——serde_yaml 不支持默认的 externally-tagged 变体（会报
/// `expected a YAML tag starting with '!'`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InsertPosition {
    /// 方向：`before` 或 `after`。
    pub direction: Direction,
    /// 锚点列名（必须为基础列）。
    pub anchor: String,
}

/// 插入方向。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Before,
    After,
}

impl InsertPosition {
    /// 便捷构造：锚点列之前。
    pub fn before(anchor: impl Into<String>) -> Self {
        InsertPosition {
            direction: Direction::Before,
            anchor: anchor.into(),
        }
    }
    /// 便捷构造：锚点列之后。
    pub fn after(anchor: impl Into<String>) -> Self {
        InsertPosition {
            direction: Direction::After,
            anchor: anchor.into(),
        }
    }
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
/// 算法：每个 MappingColumn 解析出目标 index（Before(X)→X 的 index，
/// After(X)→X 的 index + 1）。**位置锚点 X 必须是基础列之一**（PRD §2.3
/// 锚点约束）——不允许以其它映射列为锚点。因此所有目标 index 由基础列布局
/// 唯一确定、互不影响，一次性计算即可。按 index 升序、同 index 按 config
/// 顺序从后往前插入到 `result`（保持同 index 列按配置顺序堆叠）。
/// 锚点不在基础列中时该列追加到末尾。
pub fn compute_column_order(
    base: &[&str],
    mapping_cols: &[MappingColumn],
) -> Vec<String> {
    let mut result: Vec<String> = base.iter().map(|s| s.to_string()).collect();
    // 目标 index 仅取决于基础列（锚点被约束为基础列），互不影响
    let mut placements: Vec<(usize, String)> = mapping_cols
        .iter()
        .map(|c| {
            match base.iter().position(|x| *x == c.position.anchor) {
                Some(idx) => {
                    let target = match c.position.direction {
                        Direction::Before => idx,
                        Direction::After => idx + 1,
                    };
                    (target, c.rename.clone())
                }
                None => (result.len(), c.rename.clone()),
            }
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
    let range = book.worksheet_range(&name).map_err(|e| AppError::Mapping {
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
        let cfg = MappingConfig {
            enabled: true,
            source_path: "".into(),
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
        inject_key(&mut a1, &cfg.match_keys);
        let assets = vec![a1];

        let hit = join_record(&rec("1.1.1.1", "0"), &assets, &cfg);
        assert_eq!(hit.get("机房").unwrap(), "北京A");

        let miss = join_record(&rec("2.2.2.2", "0"), &assets, &cfg);
        assert!(miss.is_empty());
    }
}
```

> 注意：PRD §2.3 锚点约束要求位置锚点必须是基础列，不允许以映射列为锚点。因此所有目标 index 由基础列布局唯一确定、互不影响，一次性计算后从后往前插入即可。同 index（多列锚定到同一基础列）的列按配置顺序堆叠：从后往前插入时先放最后的列、最后放最前的列，使配置顺序在结果中保留。

- [ ] **Step 2: `src/main.rs` 加 `mod mapper;`**

```rust
/// 程序入口（后续任务填充）。
mod devices;
mod error;
mod highlight;
mod mapper;
mod processor;

fn main() {
    println!("gpu-util-monitor: 脚手架就绪");
}
```

- [ ] **Step 3: 运行测试验证通过**

Run: `cargo test mapper`
Expected: 4 个测试 PASS。

- [ ] **Step 4: 提交**

```bash
git add src/mapper.rs src/main.rs
git commit -m "feat(mapper): 资产表加载、@key join 与列位置排布"
```

---

## Task 8: config 模块（YAML 解析 + 默认生成 + CLI 合并）

**Files:**
- Create: `src/config.rs`
- Modify: `src/main.rs`（加 `mod config;`）

- [ ] **Step 1: 写实现 `src/config.rs`**

```rust
//! 配置模块：YAML 反序列化、带中文注释的默认配置生成、CLI 参数合并。
//!
//! 对应设计文档 §4.1。所有子结构都 derive `Serialize`/`Deserialize`，
//! 默认配置模板通过 [`default_config_yaml`] 产出。

use crate::devices::{ascend_910b_spec, nvidia_a10_spec, DeviceSpec};
use crate::highlight::ThresholdTriggers;
use crate::mapper::MappingConfig;
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// CLI 可覆盖的运行参数（来自 clap）。
#[derive(Debug, Clone, Default)]
pub struct CliOverrides {
    pub start: Option<String>,
    pub end: Option<String>,
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
pub fn default_config_yaml() -> String {
    format!(
        r#"# === GPU/NPU 利用率监控 默认配置 ===
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
{nvidia}
  ascend_910b:
{ascend}

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
"#,
        nvidia = indent_device(4, &nvidia_a10_spec()),
        ascend = indent_device(4, &ascend_910b_spec()),
    )
}

/// 把 DeviceSpec 序列化后按 `level` 层（每层 2 空格）缩进，嵌入到 `key:` 下方。
/// serde_yaml 顶层会带一个 `---` 文档标记，需去掉。
fn indent_device(level: usize, spec: &DeviceSpec) -> String {
    let yaml = serde_yaml::to_string(spec).unwrap_or_default();
    let pad = " ".repeat(level * 2);
    yaml.lines()
        .filter(|l| !l.trim_start().starts_with("---"))
        .map(|l| format!("{pad}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 加载配置：若路径不存在则写出默认并返回特殊 Ok(None) 让 main 提示退出。
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
            &CliOverrides { start: Some("2026-01-01 00:00:00".into()), end: None, config_path: None, output: None },
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
```

- [ ] **Step 2: `src/main.rs` 加 `mod config;`**

```rust
/// 程序入口（后续任务填充）。
mod config;
mod devices;
mod error;
mod highlight;
mod mapper;
mod processor;

fn main() {
    println!("gpu-util-monitor: 脚手架就绪");
}
```

- [ ] **Step 3: 运行测试验证通过**

Run: `cargo test config`
Expected: 4 个测试 PASS。（默认模板 thresholds 全为 `null`，反序列化为 `None`，`default_yaml_round_trips` 据此断言。）

- [ ] **Step 4: 提交**

```bash
git add src/config.rs src/main.rs
git commit -m "feat(config): YAML 解析、默认模板生成与 CLI 覆盖"
```

---

## Task 9: fetcher 模块（MetricFetcher trait + PrometheusFetcher）

**Files:**
- Create: `src/fetcher.rs`
- Modify: `src/main.rs`（加 `mod fetcher;`）

- [ ] **Step 1: 写实现 `src/fetcher.rs`**

```rust
//! 数据源适配层模块。
//!
//! 通过 [`MetricFetcher`] trait 抽象"查询某 PromQL 在时间范围内的时序"，
//! 具体实现 [`PrometheusFetcher`] 走 HTTP `/api/v1/query_range`。
//! fetcher 还负责把 [`DeviceSpec`](crate::devices::DeviceSpec) 翻译成 PromQL。

use crate::error::AppError;
use crate::processor::Series;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;

/// fetcher 查询的抽象接口，便于测试用 mock 替换真实 HTTP。
#[async_trait]
pub trait MetricFetcher: Send + Sync {
    /// range query：返回多条带标签的时序。
    async fn query_range(
        &self,
        promql: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        step: Duration,
    ) -> Result<Vec<Series>, AppError>;

    /// instant query：返回当前时刻的标签值集合。
    async fn query_instant(
        &self,
        promql: &str,
    ) -> Result<Vec<Series>, AppError>;
}

/// 调用 Prometheus HTTP API 的实现。
pub struct PrometheusFetcher {
    client: reqwest::Client,
    base_url: String,
    timeout: std::time::Duration,
    /// 用于错误提示的数据源别名。
    source_name: String,
}

impl PrometheusFetcher {
    pub fn new(source_name: String, base_url: String, timeout_secs: u64) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url,
            timeout: std::time::Duration::from_secs(timeout_secs),
            source_name,
        }
    }
}

/// Prometheus `/api/v1/query_range` 的 JSON 响应（仅取需要的字段）。
#[derive(serde::Deserialize)]
struct PromResponse {
    status: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    data: Option<PromData>,
}

#[derive(serde::Deserialize)]
struct PromData {
    #[serde(default)]
    result: Vec<PromResult>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum PromResult {
    Matrix {
        metric: HashMap<String, String>,
        values: Vec<(f64, String)>, // (unix_ts, value 字符串)
    },
    Vector {
        metric: HashMap<String, String>,
        value: (f64, String),
    },
}

#[async_trait]
impl MetricFetcher for PrometheusFetcher {
    async fn query_range(
        &self,
        promql: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        step: Duration,
    ) -> Result<Vec<Series>, AppError> {
        let url = format!(
            "{}/api/v1/query_range",
            self.base_url.trim_end_matches('/')
        );
        let resp = self
            .client
            .get(&url)
            .query(&[
                ("query", promql),
                ("start", &start.timestamp().to_string()),
                ("end", &end.timestamp().to_string()),
                ("step", &step.num_seconds().to_string()),
            ])
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| AppError::Prometheus {
                source: self.source_name.clone(),
                url: self.base_url.clone(),
                detail: format!("连接失败：{e}"),
            })?;
        let body: PromResponse = resp.json().await.map_err(|e| AppError::Prometheus {
            source: self.source_name.clone(),
            url: self.base_url.clone(),
            detail: format!("解析响应失败：{e}"),
        })?;
        parse_response(body, &self.source_name)
    }

    async fn query_instant(
        &self,
        promql: &str,
    ) -> Result<Vec<Series>, AppError> {
        let url = format!("{}/api/v1/query", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .get(&url)
            .query(&[("query", promql)])
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| AppError::Prometheus {
                source: self.source_name.clone(),
                url: self.base_url.clone(),
                detail: format!("连接失败：{e}"),
            })?;
        let body: PromResponse = resp.json().await.map_err(|e| AppError::Prometheus {
            source: self.source_name.clone(),
            url: self.base_url.clone(),
            detail: format!("解析响应失败：{e}"),
        })?;
        parse_response(body, &self.source_name)
    }
}

/// 把 Prometheus 响应转成 Series 列表。Vector 形式当作单点序列。
fn parse_response(resp: PromResponse, source: &str) -> Result<Vec<Series>, AppError> {
    if resp.status != "success" {
        return Err(AppError::Promql {
            source: source.into(),
            detail: resp.error.unwrap_or_else(|| "未知错误".into()),
        });
    }
    let data = resp.data.unwrap_or(PromData { result: vec![] });
    let mut out = Vec::new();
    for r in data.result {
        match r {
            PromResult::Matrix { metric, values } => {
                let mut points = Vec::with_capacity(values.len());
                for (ts, val) in values {
                    if let Ok(v) = val.parse::<f64>() {
                        if let Some(dt) = DateTime::<Utc>::from_timestamp(ts as i64, 0) {
                            points.push((dt, v));
                        }
                    }
                }
                out.push(Series { labels: metric, points });
            }
            PromResult::Vector { metric, value } => {
                if let Ok(v) = value.1.parse::<f64>() {
                    if let Some(dt) = DateTime::<Utc>::from_timestamp(value.0 as i64, 0) {
                        out.push(Series {
                            labels: metric,
                            points: vec![(dt, v)],
                        });
                    }
                }
            }
        }
    }
    Ok(out)
}

/// 由 GPU 显存策略生成单条 PromQL（FB_USED/(FB_USED+FB_FREE)*100）。
pub fn gpu_memory_promql(used: &str, free: &str) -> String {
    format!("{used} / ({used} + {free}) * 100")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_matrix_response() {
        let resp = PromResponse {
            status: "success".into(),
            error: None,
            data: Some(PromData {
                result: vec![PromResult::Matrix {
                    metric: HashMap::from([("gpu".into(), "0".into())]),
                    values: vec![(1000.0, "50.0".into()), (1060.0, "75.0".into())],
                }],
            }),
        };
        let s = parse_response(resp, "src").unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].points.len(), 2);
        assert!((s[0].points[1].1 - 75.0).abs() < 1e-9);
    }

    #[test]
    fn parse_error_status() {
        let resp = PromResponse {
            status: "error".into(),
            error: Some("bad_data".into()),
            data: None,
        };
        assert!(parse_response(resp, "src").is_err());
    }

    #[test]
    fn gpu_memory_promql_format() {
        let q = gpu_memory_promql("DCGM_FI_DEV_FB_USED", "DCGM_FI_DEV_FB_FREE");
        assert!(q.contains("DCGM_FI_DEV_FB_USED / (DCGM_FI_DEV_FB_USED + DCGM_FI_DEV_FB_FREE) * 100"));
    }

    /// Mock fetcher：返回预设序列，用于编排逻辑测试（不连真实 Prometheus）。
    pub struct MockFetcher {
        pub series: Vec<Series>,
    }

    #[async_trait]
    impl MetricFetcher for MockFetcher {
        async fn query_range(
            &self,
            _promql: &str,
            _start: DateTime<Utc>,
            _end: DateTime<Utc>,
            _step: Duration,
        ) -> Result<Vec<Series>, AppError> {
            Ok(self.series.clone())
        }
        async fn query_instant(&self, _promql: &str) -> Result<Vec<Series>, AppError> {
            Ok(self.series.clone())
        }
    }
}
```

- [ ] **Step 2: `src/main.rs` 加 `mod fetcher;`**

```rust
/// 程序入口（后续任务填充）。
mod config;
mod devices;
mod error;
mod fetcher;
mod highlight;
mod mapper;
mod processor;

fn main() {
    println!("gpu-util-monitor: 脚手架就绪");
}
```

- [ ] **Step 3: 运行测试验证通过**

Run: `cargo test fetcher`
Expected: 3 个测试 PASS（MockFetcher 不计为测试）。

- [ ] **Step 4: 提交**

```bash
git add src/fetcher.rs src/main.rs
git commit -m "feat(fetcher): MetricFetcher trait 与 Prometheus HTTP 实现"
```

---

## Task 10: reporter 模块（Excel 渲染 + 染色集成）

**Files:**
- Create: `src/reporter.rs`
- Modify: `src/main.rs`（加 `mod reporter;`）

- [ ] **Step 1: 写实现 `src/reporter.rs`**

```rust
//! 报告渲染层模块。
//!
//! 隔离 rust_xlsxwriter，专职把 `Vec<CardRecord>` + 列布局 + 染色决策写成带样式的
//! `.xlsx`。基础列顺序来自 [`mapper::BASE_COLUMNS`](crate::mapper::BASE_COLUMNS)，
//! 映射列位置由 mapper 计算后传入。

use crate::error::AppError;
use crate::highlight::{HexColor, ThresholdTriggers};
use crate::mapper::compute_column_order;
use crate::processor::CardRecord;
use rust_xlsxwriter::{Color, Format, Workbook};
use std::collections::HashMap;

/// 报表所有列名（基础列 + 映射列），由调用方用 mapper 计算后传入。
pub struct ReportSpec {
    pub base_columns: Vec<String>,
    /// 映射列 rename 清单（顺序即追加顺序，reporter 仅按 compute_column_order 排）。
    pub mapping_renames: Vec<String>,
}

/// 把记录写为 .xlsx 字节缓冲。返回内存 buffer，由 main 落盘或测试断言。
///
/// - 首行冻结 + 加粗 + 深蓝底白字
/// - 利用率列存为 value/100，数字格式 0.00%；N/A 写字符串 "N/A"
/// - 时间列 yyyy-mm-dd hh:mm:ss
/// - 命中染色单元格套对应 HEX 背景色 Format
pub fn render_to_buffer(
    records: &[CardRecord],
    spec: &ReportSpec,
    mapping_columns: &[crate::mapper::MappingColumn],
    thresholds: &ThresholdTriggers,
    mapping_values: &HashMap<(usize, String), String>, // (行索引, rename) -> 资产值
) -> Result<Vec<u8>, AppError> {
    let base_refs: Vec<&str> = spec.base_columns.iter().map(|s| s.as_str()).collect();
    let order = compute_column_order(&base_refs, mapping_columns);

    let mut wb = Workbook::new();
    let sheet = wb.add_worksheet().set_name("利用率报表").map_err(|e| AppError::Report {
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
            .map_err(|e| AppError::Report { detail: format!("写表头失败：{e}") })?;
    }
    sheet
        .set_freeze_panes(1, 0)
        .map_err(|e| AppError::Report { detail: format!("冻结首行失败：{e}") })?;

    // 命中染色单元格的百分比 Format 构造器（按 HEX）
    let pct_color = |hex: &HexColor| -> Format {
        let rgb = u32::from_str_radix(&hex.0[1..], 16).unwrap_or(0xFF0000);
        Format::new()
            .set_background_color(Color::RGB(rgb))
            .set_num_format("0.00%")
    };

    for (row_idx, rec) in records.iter().enumerate() {
        let excel_row = (row_idx + 1) as u16;
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
                        .map_err(|e| AppError::Report { detail: format!("写单元格失败：{e}") })?;
                }
                CellValue::Text(t) => {
                    sheet
                        .write_string(excel_row, col, t)
                        .map_err(|e| AppError::Report { detail: format!("写文本失败：{e}") })?;
                }
                CellValue::Na => {
                    sheet
                        .write_string(excel_row, col, "N/A")
                        .map_err(|e| AppError::Report { detail: format!("写 N/A 失败：{e}") })?;
                }
            }
        }
    }

    // 列宽自适应（简单按列名长度与内容估算）
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
    let ts = |dt: chrono::DateTime<chrono::Utc>| {
        dt.format("%Y-%m-%d %H:%M:%S").to_string()
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
            rec.range_start.format("%Y-%m-%d %H:%M:%S"),
            rec.range_end.format("%Y-%m-%d %H:%M:%S")
        )),
        "核心利用率平均值" => rec.core_avg.map(CellValue::Pct).unwrap_or(CellValue::Na),
        "核心利用率峰值" => rec.core_peak.map(CellValue::Pct).unwrap_or(CellValue::Na),
        "核心利用率峰值出现时间" => rec.core_peak_time.map(ts).map(CellValue::Text).unwrap_or(CellValue::Na),
        "显存占用率平均值" => rec.mem_avg.map(CellValue::Pct).unwrap_or(CellValue::Na),
        "显存占用率峰值" => rec.mem_peak.map(CellValue::Pct).unwrap_or(CellValue::Na),
        "显存占用率峰值出现时间" => rec.mem_peak_time.map(ts).map(CellValue::Text).unwrap_or(CellValue::Na),
        other => {
            // 映射列
            match mapping_values.get(&(row_idx, other.to_string())) {
                Some(v) => CellValue::Text(v.clone()),
                None => CellValue::Text(String::new()),
            }
        }
    }
}
```

> 说明：`render_to_buffer` 的 `mapping_columns` 参数用于 `compute_column_order` 计算列顺序；`ReportSpec.mapping_renames` 当前未被 render 直接使用（列顺序已由 compute_column_order 决定），保留以备未来校验。若编译器报未使用警告，可在函数末尾加 `let _ = &spec.mapping_renames;`。

- [ ] **Step 2: `src/main.rs` 加 `mod reporter;`**

```rust
/// 程序入口（后续任务填充）。
mod config;
mod devices;
mod error;
mod fetcher;
mod highlight;
mod mapper;
mod processor;
mod reporter;

fn main() {
    println!("gpu-util-monitor: 脚手架就绪");
}
```

- [ ] **Step 3: 编译检查（reporter 本身不写单测，依赖 calamine 读回测试放在集成任务）**

Run: `cargo build`
Expected: `Finished` 无错误。

> 时间列以 `Text`（格式化字符串）写入，规避了 `rust_xlsxwriter` 0.79 中 chrono 类型不实现 `IntoExcelDateTime` 的限制，满足 PRD §3 的显示要求。

- [ ] **Step 4: 提交**

```bash
git add src/reporter.rs src/main.rs
git commit -m "feat(reporter): Excel 渲染与染色集成"
```

---

## Task 11: main 编排（CLI + 流水线串联）

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: 写 `src/main.rs`**

```rust
//! GPU/NPU 多源利用率监控 CLI 入口。
//!
//! 编排流水线：config → fetcher → processor → mapper → highlight → reporter。
//! 单源/单卡失败降级为 N/A，仅打印警告；致命错误打印中文提示并退出码 1。

mod config;
mod devices;
mod error;
mod fetcher;
mod highlight;
mod mapper;
mod processor;
mod reporter;

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use clap::Parser;
use config::{AppConfig, CliOverrides};
use error::AppError;
use fetcher::{MetricFetcher, PrometheusFetcher};
use highlight::ThresholdTriggers;
use processor::{aggregate, CardRecord};
use std::collections::HashMap;
use std::process::ExitCode;

/// CLI 参数。
#[derive(Parser, Debug)]
#[command(name = "gpu-util-monitor", about = "GPU/NPU 利用率监控与报表生成")]
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
            println!("[提示] 未发现配置文件，已在 {} 生成默认配置，请编辑后重新运行。", args.config);
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
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let end = match parse_time(&cfg.time_range.end) {
        Ok(t) => t,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let step = Duration::seconds(cfg.report.query_step_secs as i64);

    // 3. 采集 + 聚合
    let mut warnings: Vec<String> = Vec::new();
    let mut records: Vec<CardRecord> = Vec::new();
    for src in &cfg.sources {
        let fetcher = PrometheusFetcher::new(src.name.clone(), src.url.clone(), src.timeout_secs);
        for dt_key in &src.device_types {
            let spec = match cfg.devices.get(dt_key) {
                Some(s) => s.clone(),
                None => {
                    warnings.push(format!("数据源 {} 引用了未定义的设备类型 {}", src.name, dt_key));
                    continue;
                }
            };
            match collect_device(&fetcher, &src.name, &spec, start, end, step, &cfg).await {
                Ok(mut recs) => records.append(&mut recs),
                Err(e) => warnings.push(format!("{e}")),
            }
        }
    }

    // 4. 资产映射（可选）
    let mut mapping_values: HashMap<(usize, String), String> = HashMap::new();
    let mapping_columns: Vec<mapper::MappingColumn> = if let Some(m) = &cfg.mapping {
        if m.enabled {
            match mapper::load_asset_table(&m.source_path, &m.match_keys) {
                Ok(assets) => {
                    for (i, rec) in records.iter().enumerate() {
                        let joined = mapper::join_record(rec, &assets, m);
                        for (rename, val) in joined {
                            mapping_values.insert((i, rename), val);
                        }
                    }
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

    // 5. 渲染
    let spec = reporter::ReportSpec {
        base_columns: mapper::BASE_COLUMNS.iter().map(|s| s.to_string()).collect(),
        mapping_renames: mapping_columns.iter().map(|c| c.rename.clone()).collect(),
    };
    match reporter::render_to_buffer(&records, &spec, &mapping_columns, &cfg.thresholds, &mapping_values) {
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

/// 采集一个设备类型在一个源上的所有卡，聚合成 CardRecord 列表。
async fn collect_device(
    fetcher: &dyn MetricFetcher,
    source_name: &str,
    spec: &devices::DeviceSpec,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step: Duration,
    cfg: &AppConfig,
) -> Result<Vec<CardRecord>, AppError> {
    use devices::MemoryStrategy;
    use processor::{hbm_fallback_series, Series};

    // 核心利用率
    let core_series = fetcher
        .query_range(&spec.core_util_metric, start, end, step)
        .await
        .unwrap_or_default();

    // 显存：根据策略
    let mem_series: Vec<Series> = match &spec.memory {
        MemoryStrategy::CompositeRatio(b) => {
            let q = fetcher::gpu_memory_promql(&b.composite_ratio.used, &b.composite_ratio.free);
            fetcher.query_range(&q, start, end, step).await.unwrap_or_default()
        }
        MemoryStrategy::DirectMetric(b) => {
            fetcher.query_range(&b.direct_metric.metric, start, end, step).await.unwrap_or_default()
        }
        MemoryStrategy::CompositeFromTotal(_) => vec![],
    };

    // NPU fallback：direct 为空时拉 used/total
    let mut effective_mem = mem_series.clone();
    if effective_mem.is_empty() {
        if let MemoryStrategy::DirectMetric(b) = &spec.memory {
            if let Some(fb) = &b.direct_metric.fallback {
                if let MemoryStrategy::CompositeFromTotal(body) = fb.as_ref() {
                    let used = &body.composite_from_total.used;
                    let total = &body.composite_from_total.total;
                    let used_s = fetcher.query_range(used, start, end, step).await.unwrap_or_default();
                    let total_s = fetcher.query_range(total, start, end, step).await.unwrap_or_default();
                    effective_mem = used_s
                        .iter()
                        .map(|u| {
                            hbm_fallback_series(u, &total_s.iter().find(|t| t.labels == u.labels).cloned().unwrap_or_default())
                        })
                        .collect();
                }
            }
        }
    }

    // 按 (host_ip, card_id) 分组聚合
    let mut groups: HashMap<String, (Series, Option<Series>)> = HashMap::new();
    for s in core_series {
        let key = series_key(&s, spec, cfg);
        groups.entry(key).or_insert_with(|| (s.clone(), None)).0 = s;
    }
    for s in effective_mem {
        let key = series_key(&s, spec, cfg);
        groups.entry(key).or_insert_with(|| (Series { labels: s.labels.clone(), points: vec![] }, None)).1 = Some(s);
    }

    let mut out = Vec::new();
    for (_, (core, mem)) in groups {
        let host_ip = extract_ip(&core.labels, &cfg.host_ip.prefer_label);
        let card_id = core.labels.get(&spec.card_id_label).cloned().unwrap_or_default();
        let node_name = core.labels.get("node").cloned().unwrap_or_default();
        let (c_avg, c_peak, c_peak_t) = stat3(&core.points);
        let (m_avg, m_peak, m_peak_t) = mem
            .as_ref()
            .map(|m| stat3(&m.points))
            .unwrap_or((None, None, None));

        // 归属（末态简化：取标签瞬时值；完整 last_in_range 见 processor::last_non_empty）
        let namespace = core.labels.get(&spec.labels.namespace).cloned().unwrap_or_default();
        let pod = core.labels.get(&spec.labels.pod).cloned().unwrap_or_default();
        let container = core.labels.get(&spec.labels.container).cloned().unwrap_or_default();

        out.push(CardRecord {
            source_name: source_name.into(),
            host_ip,
            node_name,
            card_id,
            device_type: spec.display_name.clone(),
            namespace,
            pod,
            container,
            core_avg: c_avg,
            core_peak: c_peak,
            core_peak_time: c_peak_t,
            mem_avg: m_avg,
            mem_peak: m_peak,
            mem_peak_time: m_peak_t,
            range_start: start,
            range_end: end,
        });
    }
    Ok(out)
}

/// 把一组点聚合成 (avg, peak, peak_time)，空则全 None。
fn stat3(points: &[(DateTime<Utc>, f64)]) -> (Option<f64>, Option<f64>, Option<DateTime<Utc>>) {
    match aggregate(points) {
        Some(s) => (Some(s.avg), Some(s.peak), Some(s.peak_time)),
        None => (None, None, None),
    }
}

/// 序列分组 key：host_ip + card_id。
fn series_key(s: &processor::Series, spec: &devices::DeviceSpec, cfg: &AppConfig) -> String {
    let ip = extract_ip(&s.labels, &cfg.host_ip.prefer_label);
    let card = s.labels.get(&spec.card_id_label).cloned().unwrap_or_default();
    format!("{ip}|{card}")
}

/// 从标签取主机 IP：优先 prefer_label，否则 instance 去端口。
fn extract_ip(labels: &HashMap<String, String>, prefer: &str) -> String {
    if let Some(v) = labels.get(prefer) {
        if !v.is_empty() {
            return v.clone();
        }
    }
    labels
        .get("instance")
        .map(|s| s.rsplit_once(':').map(|(h, _)| h.to_string()).unwrap_or_else(|| s.clone()))
        .unwrap_or_default()
}

fn parse_time(s: &str) -> Result<DateTime<Utc>, AppError> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
        .map(|ndt| DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
        .map_err(|_| AppError::TimeFormat { raw: s.into() })
}
```

- [ ] **Step 2: 编译**

Run: `cargo build`
Expected: `Finished` 无错误。

- [ ] **Step 3: 提交**

```bash
git add src/main.rs
git commit -m "feat(main): CLI 编排与采集/聚合/映射/渲染流水线"
```

---

## Task 12: 集成验证（lib 拆分 + 端到端冒烟）

**Files:**
- Modify: `Cargo.toml`（加 `[lib]` + `[[bin]]`）
- Create: `src/lib.rs`
- Modify: `src/main.rs`（移除 `mod xxx;`，改用 `use gpu_util_monitor::*;`）
- Create: `tests/e2e_render.rs`

> 先做 lib/bin 拆分，使集成测试（`tests/`）能引用库符号；再写端到端渲染测试。

- [ ] **Step 1: 把 Cargo.toml 改为同时支持 lib + bin**

把 `[package]` 下方加上 `[lib]` 与 `[[bin]]`，依赖区块保持原样：

```toml
[package]
name = "gpu-util-monitor"
version = "0.1.0"
edition = "2021"

[lib]
name = "gpu_util_monitor"
path = "src/lib.rs"

[[bin]]
name = "gpu-util-monitor"
path = "src/main.rs"

[dependencies]
tokio = { version = "1", features = ["full"] }
clap = { version = "4", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
serde_yaml = "0.9"
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
chrono = { version = "0.4", features = ["serde"] }
thiserror = "1"
anyhow = "1"
csv = "1"
calamine = "0.26"
rust_xlsxwriter = "0.79"
futures = "0.3"
async-trait = "0.1"
```

- [ ] **Step 2: 新建 `src/lib.rs`，把所有模块声明集中到此并 pub**

```rust
//! 库入口：把各模块 pub 出去，供集成测试与未来复用。
//! main.rs（bin）与 tests/ 均通过 `gpu_util_monitor::` 路径引用。

pub mod config;
pub mod devices;
pub mod error;
pub mod fetcher;
pub mod highlight;
pub mod mapper;
pub mod processor;
pub mod reporter;
```

- [ ] **Step 3: 改造 `src/main.rs`——删除顶部所有 `mod xxx;`，改为引用 lib**

把 `src/main.rs` 顶部的 8 行 `mod xxx;`（`mod config;` … `mod reporter;`）**全部删除**，替换为：

```rust
//! GPU/NPU 多源利用率监控 CLI 入口。
//!
//! 编排流水线：config → fetcher → processor → mapper → highlight → reporter。
//! 单源/单卡失败降级为 N/A，仅打印警告；致命错误打印中文提示并退出码 1。

use gpu_util_monitor::config;
use gpu_util_monitor::devices;
use gpu_util_monitor::error::AppError;
use gpu_util_monitor::fetcher::{self, MetricFetcher, PrometheusFetcher};
use gpu_util_monitor::mapper;
use gpu_util_monitor::processor::{self, aggregate, CardRecord, Series};

use chrono::{DateTime, Duration, NaiveDateTime, Utc};
use clap::Parser;
use config::{AppConfig, CliOverrides};
use std::collections::HashMap;
use std::process::ExitCode;
```

`main.rs` 函数体里其余对 `config::`、`devices::`、`fetcher::`、`mapper::`、`processor::` 的引用无需改动（上面的 `use` 已把它们导入为模块路径根）。`highlight::ThresholdTriggers` 未在 main 直接出现（thresholds 走 `cfg.thresholds` 字段），无需额外 use。

- [ ] **Step 4: 验证 lib + bin 仍编译通过**

Run: `cargo build`
Expected: `Finished` 无错误。

- [ ] **Step 5: 写端到端渲染测试 `tests/e2e_render.rs`**

```rust
use chrono::TimeZone;
use chrono::Utc;
use gpu_util_monitor::highlight::{HexColor, ThresholdTriggers, TriggerConfig};
use gpu_util_monitor::mapper::{BASE_COLUMNS};
use gpu_util_monitor::processor::CardRecord;
use gpu_util_monitor::reporter::{render_to_buffer, ReportSpec};
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
    let mut tr = ThresholdTriggers::default();
    tr.core_avg_above = Some(TriggerConfig {
        enabled: true,
        threshold: 80.0,
        color: HexColor("#FF0000".into()),
    });
    let spec = ReportSpec {
        base_columns: BASE_COLUMNS.iter().map(|s| s.to_string()).collect(),
        mapping_renames: vec![],
    };
    let buf = render_to_buffer(&[rec], &spec, &[], &tr, &HashMap::new()).unwrap();
    assert!(buf.len() > 1000, "应生成非空 xlsx 字节");

    // 用 calamine 读回断言行数（calamine 0.26 不暴露填充色，染色命中由 highlight 单测覆盖）
    use calamine::{open_workbook_from_rs, Reader, Xlsx};
    let r: Xlsx<_> = open_workbook_from_rs(std::io::Cursor::new(buf)).unwrap();
    let name = r.sheet_names()[0].clone();
    let range = r.worksheet_range(&name).unwrap();
    assert_eq!(range.height(), 2, "1 表头 + 1 数据行");
}
```

- [ ] **Step 6: 运行集成测试**

Run: `cargo test --test e2e_render`
Expected: PASS。

- [ ] **Step 7: 生成默认配置并检查（开箱即用）**

Run:
```bash
cd /tmp && rm -rf gputest && mkdir gputest && cd gputest
cargo run --manifest-path "/Volumes/Data/Projects/Prometheus计算卡利用率统计/Cargo.toml" -- --config ./config.yaml
head -5 ./config.yaml
```
Expected: 输出 `[提示] 未发现配置文件...`，且 `config.yaml` 存在并首行含 `time_range:`。

- [ ] **Step 8: 全量编译与测试**

Run: `cargo build && cargo test`
Expected: 全部 PASS。

- [ ] **Step 9: 提交**

```bash
git add Cargo.toml src/lib.rs src/main.rs tests/e2e_render.rs
git commit -m "test: lib/bin 拆分与端到端渲染集成测试"
```

---

## 完成标准 (Definition of Done)

- [ ] `cargo build` 无错误、无 warning（除允许的外部 crate warning）
- [ ] `cargo test` 全绿
- [ ] 运行二进制可在无配置时生成默认 `config.yaml`（开箱即用）
- [ ] 所有 struct/trait/复杂函数有中文 doc 注释
- [ ] 无 `unwrap`/`expect`/`panic!`（用 `grep -rn "unwrap()\|expect(\|panic!" src/` 核对，仅测试代码允许）
- [ ] 高内聚低耦合：六模块单向依赖，无环
