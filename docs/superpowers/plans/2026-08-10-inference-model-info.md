# 模型名称列 + inference_model_info 采集 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增「模型名称」报表列（容器名称之后），每个数据源查询一次 `inference_model_info` 指标构建 (namespace, pod) → 模型名映射，按"标签优先、unknown/未匹配时从 pod 名取前 3 段推导、推导失败写未知"规则填充每行。

**Architecture:** 数据流保持单向：`config → main（每源建 fetcher，查询一次模型映射）→ pipeline（collect_device 产出记录后 apply_model_info 填充）→ mapper/reporter（基础列渲染）`。`collect_device` 签名不变，模型填充作为后置 pass 在 main.rs 完成；推导逻辑是纯函数，便于单测。

**Tech Stack:** Rust 2021、tokio、reqwest、serde_yaml_ng、rust_xlsxwriter。现有 `MetricFetcher` trait + `MockFetcher` 测试替身。

## Global Constraints

- 版本号必须同步更新：`Cargo.toml` → `1.10.0`（MINOR，新功能）
- 每次任务完成后 `git commit` + `git push`（项目 CLAUDE.md 推送规则）
- 非必要不修改 `.github/workflows/` 下的文件
- `cargo test` 全绿、`cargo clippy` 零 warning（项目 README 开发标准）
- 遵循现有代码风格：中文注释、`#[serde(deny_unknown_fields)]`、`#[must_use]`、新配置字段必须带 serde 默认值（向后兼容旧配置文件）
- 指标名合法性 `[a-zA-Z_:][a-zA-Z0-9_:]*`，标签名 `[a-zA-Z_][a-zA-Z0-9_]*`（复用 config.rs 现有校验函数）
- 模型名推导规则（spec §2，逐字）：pod 名按 `-` 分隔取前 3 段；不足 3 段写 `"未知"`；`inference_model` 标签值 `"unknown"` 视为无模型
- 每源查询一次模型映射；查询失败 → Warning + 空映射，全部走推导兜底（不中断）

---

### Task 1: 配置结构 `ModelInfoSpec` + `CardRecord` 新字段

**Files:**
- Modify: `src/devices.rs`（末尾、`#[cfg(test)]` 前新增结构体）
- Modify: `src/config.rs`（`SourceConfig` 加字段；`validate_config` 加校验；tests 模块加测试）
- Modify: `src/processor.rs:13`（`CardRecord` 加字段）
- Modify: 7 处 `CardRecord` 字面量补字段：`src/pipeline.rs:282`、`src/mapper.rs:839`、`src/highlight.rs:341`、`src/reporter.rs:486`、`src/reporter.rs:558`、`tests/e2e_render.rs:22`、`tests/e2e_render.rs:92`

**Interfaces:**
- Produces: `devices::ModelInfoSpec { enabled: bool, metric: String, model_label: String }`（三个字段全部带 serde 默认值：`enabled=true`、`metric="inference_model_info"`、`model_label="inference_model"`）；`SourceConfig.model_info: Option<ModelInfoSpec>`（`#[serde(default)]`）；`CardRecord.inference_model: String`——后续任务依赖这些类型。

- [ ] **Step 1: 写失败测试（config.rs tests 模块）**

在 `src/config.rs` 的 `mod tests` 内追加：

```rust
#[test]
fn model_info_rejects_invalid_metric_name() {
    let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
    cfg.sources[0].model_info = Some(crate::devices::ModelInfoSpec {
        enabled: true,
        metric: "metric{evil=\"yes\"}".into(),
        model_label: "inference_model".into(),
    });
    let r = validate_config(&cfg, "test.yaml");
    assert!(r.is_err(), "非法 model_info.metric 应被拒绝");
    assert!(format!("{}", r.unwrap_err()).contains("model_info"), "错误信息应提及 model_info");
}

#[test]
fn model_info_rejects_invalid_model_label() {
    let mut cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
    cfg.sources[0].model_info = Some(crate::devices::ModelInfoSpec {
        enabled: true,
        metric: "inference_model_info".into(),
        model_label: "bad:label".into(),
    });
    let r = validate_config(&cfg, "test.yaml");
    assert!(r.is_err(), "非法 model_info.model_label 应被拒绝");
}

#[test]
fn model_info_defaults_round_trip() {
    // 默认模板不含 model_info → Option 为 None，旧配置不受影响
    let cfg = serde_yaml_ng::from_str::<AppConfig>(&default_config_yaml()).unwrap();
    assert!(cfg.sources[0].model_info.is_none());
}

#[test]
fn model_info_serde_defaults() {
    // 只写 enabled，metric/model_label 走默认值
    let s: crate::devices::ModelInfoSpec = serde_yaml_ng::from_str("enabled: false\n").unwrap();
    assert!(!s.enabled);
    assert_eq!(s.metric, "inference_model_info");
    assert_eq!(s.model_label, "inference_model");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --lib model_info`
Expected: 编译错误——`SourceConfig` 无 `model_info` 字段 / `ModelInfoSpec` 未定义。

- [ ] **Step 3: 实现结构体与字段**

`src/devices.rs`（`#[cfg(test)]` 前）新增：

```rust
/// 模型名称采集配置（数据源级，可选）。
///
/// 查询 `inference_model_info` 类指标（值为 1 的 gauge，标签携带 pod 的
/// 推理模型名），构建 (namespace, pod) → 模型名映射，供「模型名称」列填充。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelInfoSpec {
    /// 是否启用模型信息采集（默认 true）。false 仅跳过指标查询，
    /// 报表列仍显示从 pod 名推导的名称。
    #[serde(default = "default_model_info_enabled")]
    pub enabled: bool,
    /// 模型信息指标名（默认 "inference_model_info"）。
    #[serde(default = "default_model_metric")]
    pub metric: String,
    /// 模型名所在标签名（默认 "inference_model"）。
    #[serde(default = "default_model_label")]
    pub model_label: String,
}

fn default_model_info_enabled() -> bool {
    true
}

fn default_model_metric() -> String {
    "inference_model_info".into()
}

fn default_model_label() -> String {
    "inference_model".into()
}
```

`src/config.rs` 的 `SourceConfig`（`device_types` 之后）加：

```rust
    /// 模型名称采集配置（可选）。不配置 = 跳过指标查询，
    /// 「模型名称」列仍显示从 pod 名推导的名称。
    #[serde(default)]
    pub model_info: Option<crate::devices::ModelInfoSpec>,
```

`src/processor.rs` 的 `CardRecord`（`container` 字段之后）加：

```rust
    /// 模型名称（标签值或从 pod 名推导；取不到为 "未知"）。
    pub inference_model: String,
```

- [ ] **Step 4: 修复 7 处 CardRecord 字面量**

编译会报错列出全部位置。每处在 `container: ...` 之后加一行 `inference_model: String::new(),`。
涉及：`src/pipeline.rs:282`（真实构造，用 `String::new()`）、`src/mapper.rs:839`、
`src/highlight.rs:341`、`src/reporter.rs:486`、`src/reporter.rs:558`、`tests/e2e_render.rs:22`、`tests/e2e_render.rs:92`。

- [ ] **Step 5: 实现校验逻辑**

`src/config.rs` 的 `validate_config`，在 `for src in &cfg.sources` 循环内（`device_types` 去重校验之后）加：

```rust
        // 校验模型信息配置的指标名/标签名合法性
        if let Some(mi) = &src.model_info {
            if !is_valid_metric_name(&mi.metric) {
                return Err(AppError::Config {
                    path: path.into(),
                    reason: format!(
                        "数据源「{}」的 model_info.metric「{}」不是合法的 Prometheus 指标名",
                        src.name, mi.metric
                    ),
                });
            }
            if !is_valid_label_name(&mi.model_label) {
                return Err(AppError::Config {
                    path: path.into(),
                    reason: format!(
                        "数据源「{}」的 model_info.model_label「{}」不是合法的 Prometheus 标签名",
                        src.name, mi.model_label
                    ),
                });
            }
        }
```

- [ ] **Step 6: 运行测试确认通过**

Run: `cargo test --lib model_info`
Expected: PASS（3 个测试）+ 无编译错误。

- [ ] **Step 7: 提交**

```bash
git add src/devices.rs src/config.rs src/processor.rs src/mapper.rs src/highlight.rs src/reporter.rs tests/e2e_render.rs
git commit -m "feat: 新增 ModelInfoSpec 配置与 CardRecord.inference_model 字段 v1.10.0"
```

---

### Task 2: pipeline 核心逻辑（推导 + 映射构建 + 填充）

**Files:**
- Modify: `src/pipeline.rs`（`collect_device` 之后、`collect_host_metrics` 之前插入三个函数；`mod tests` 内加测试）

**Interfaces:**
- Consumes: `crate::devices::ModelInfoSpec`（Task 1）、`crate::fetcher::MetricFetcher`、`crate::processor::CardRecord`
- Produces: `pipeline::derive_model_name(pod: &str) -> String`；`pipeline::collect_model_info(fetcher: &dyn MetricFetcher, spec: &ModelInfoSpec, start: DateTime<Utc>, end: DateTime<Utc>, step: Duration) -> Result<HashMap<(String, String), Option<String>>, AppError>`（key=(namespace,pod)，值 `Some(模型名)` / `None`=unknown）；`pipeline::apply_model_info(records: &mut [CardRecord], map: &HashMap<(String, String), Option<String>>)`——Task 3 在 main.rs 调用。

- [ ] **Step 1: 写失败测试（pipeline.rs tests 模块）**

在 `src/pipeline.rs` 的 `mod tests` 内追加（`use crate::devices::ModelInfoSpec;` 放测试模块顶部）：

```rust
    // ---- 模型名称：推导 ----

    #[test]
    fn derive_model_name_takes_first_three_segments() {
        assert_eq!(
            derive_model_name("tele-tts-onnx-hanyu-v20260604-54d775747-26hhf"),
            "tele-tts-onnx"
        );
        assert_eq!(derive_model_name("a-b-c"), "a-b-c", "恰 3 段原样返回");
    }

    #[test]
    fn derive_model_name_falls_back_to_unknown() {
        assert_eq!(derive_model_name(""), "未知");
        assert_eq!(derive_model_name("a-b"), "未知", "不足 3 段");
        assert_eq!(derive_model_name("solo"), "未知", "单段");
    }

    // ---- 模型名称：apply_model_info 四分支 ----

    #[test]
    fn apply_model_info_uses_label_when_available() {
        let mut records = vec![CardRecord {
            namespace: "ns-1".into(),
            pod: "pod-a".into(),
            ..Default::default()
        }];
        let map = HashMap::from([(("ns-1".to_string(), "pod-a".to_string()), Some("qwen3-8b-mss".to_string()))]);
        apply_model_info(&mut records, &map);
        assert_eq!(records[0].inference_model, "qwen3-8b-mss");
    }

    #[test]
    fn apply_model_info_derives_when_label_unknown() {
        let mut records = vec![CardRecord {
            namespace: "ns-1".into(),
            pod: "tele-tts-onnx-hanyu-v20260604-54d775747-26hhf".into(),
            ..Default::default()
        }];
        let map = HashMap::from([(("ns-1".to_string(), "tele-tts-onnx-hanyu-v20260604-54d775747-26hhf".to_string()), None)]);
        apply_model_info(&mut records, &map);
        assert_eq!(records[0].inference_model, "tele-tts-onnx");
    }

    #[test]
    fn apply_model_info_derives_when_pod_not_in_map() {
        let mut records = vec![CardRecord {
            namespace: "ns-1".into(),
            pod: "qwen3-8b-mss-v1-7c45667677-hq6fv".into(),
            ..Default::default()
        }];
        apply_model_info(&mut records, &HashMap::new());
        assert_eq!(records[0].inference_model, "qwen3-8b-mss-v1");
    }

    #[test]
    fn apply_model_info_writes_unknown_when_derivation_fails() {
        let mut records = vec![CardRecord {
            namespace: "ns-1".into(),
            pod: "ab".into(),
            ..Default::default()
        }];
        apply_model_info(&mut records, &HashMap::new());
        assert_eq!(records[0].inference_model, "未知");
    }

    // ---- 模型名称：collect_model_info 映射构建 ----

    #[tokio::test]
    async fn collect_model_info_indexes_both_pod_labels_and_takes_latest() {
        // 同一 (ns, pod)：早的 series 模型 X，晚的 series 模型 Y → 取 Y
        let series = vec![
            Series {
                labels: labels(&[("namespace", "ns-1"), ("pod", "pod-a"), ("pod_name", "pod-a"), ("inference_model", "model-x")]),
                points: vec![(t(0), 1.0)],
            },
            Series {
                labels: labels(&[("namespace", "ns-1"), ("pod", "pod-a"), ("pod_name", "pod-a"), ("inference_model", "model-y")]),
                points: vec![(t(60), 1.0)],
            },
            // 只有 pod_name 标签的 series（NPU 风格）也应被索引
            Series {
                labels: labels(&[("namespace", "ns-2"), ("pod_name", "pod-b"), ("inference_model", "model-b")]),
                points: vec![(t(60), 1.0)],
            },
        ];
        let fetcher = MockFetcher::new().when("inference_model_info", Ok(series));
        let spec = ModelInfoSpec {
            enabled: true,
            metric: "inference_model_info".into(),
            model_label: "inference_model".into(),
        };
        let map = collect_model_info(&fetcher, &spec, t(0), t(120), Duration::seconds(60))
            .await
            .unwrap();
        assert_eq!(map.get(&("ns-1".into(), "pod-a".into())), Some(&Some("model-y".into())), "同 pod 多 series 应取最后一条");
        assert_eq!(map.get(&("ns-2".into(), "pod-b".into())), Some(&Some("model-b".into())), "pod_name 标签也应索引");
    }

    #[tokio::test]
    async fn collect_model_info_marks_unknown_and_skips_missing_labels() {
        let series = vec![
            Series {
                labels: labels(&[("namespace", "ns-1"), ("pod", "pod-u"), ("inference_model", "unknown")]),
                points: vec![(t(0), 1.0)],
            },
            // 缺 namespace 或缺 pod 标签 → 跳过
            Series {
                labels: labels(&[("pod", "pod-nons"), ("inference_model", "model-n")]),
                points: vec![(t(0), 1.0)],
            },
            // 缺 model_label → 视为 unknown（None）
            Series {
                labels: labels(&[("namespace", "ns-1"), ("pod", "pod-no-model")]),
                points: vec![(t(0), 1.0)],
            },
        ];
        let fetcher = MockFetcher::new().when("inference_model_info", Ok(series));
        let spec = ModelInfoSpec {
            enabled: true,
            metric: "inference_model_info".into(),
            model_label: "inference_model".into(),
        };
        let map = collect_model_info(&fetcher, &spec, t(0), t(120), Duration::seconds(60))
            .await
            .unwrap();
        assert_eq!(map.get(&("ns-1".into(), "pod-u".into())), Some(&None), "unknown 应以 None 存入");
        assert_eq!(map.len(), 2, "缺标签的 series 应被跳过");
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --lib derive_model_name apply_model_info collect_model_info`
Expected: FAIL——函数未定义（编译错误）。

- [ ] **Step 3: 实现三个函数**

`src/pipeline.rs`，`collect_device` 函数之后插入：

```rust
/// 模型名推导规则：pod 名按 `-` 分隔取前 3 段（保留到第 2 个连字符为止）。
///
/// 例：`tele-tts-onnx-hanyu-v20260604-54d775747-26hhf` → `tele-tts-onnx`。
/// pod 名为空或不足 3 段时返回 `"未知"`。
#[must_use]
pub fn derive_model_name(pod: &str) -> String {
    let parts: Vec<&str> = pod.split('-').collect();
    if parts.len() >= 3 {
        parts[..3].join("-")
    } else {
        "未知".into()
    }
}

/// `inference_model_info` 标签中"无模型"的哨兵值。
const UNKNOWN_MODEL_SENTINEL: &str = "unknown";

/// 查询模型信息指标，构建 `(namespace, pod) → 模型名` 映射。
///
/// - 同一 pod 出现多条 series（标签集不同）时，取时间段内有数据的最后一条
///   （按 series 最大点时间戳比较）——与 `last_in_range` 归属语义一致
/// - 指标同时带 `pod` 与 `pod_name` 标签，两个键都插入（兼容 DCGM 与 NPU
///   exporter 的不同命名）
/// - `inference_model` 值为 `"unknown"` 或标签缺失 → 以 `None` 存入
///   （与查不到等价，由 `apply_model_info` 走推导兜底）
/// - 缺 namespace/pod 标签的 series 直接跳过（无法匹配）
pub async fn collect_model_info(
    fetcher: &dyn MetricFetcher,
    spec: &ModelInfoSpec,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    step: Duration,
) -> Result<HashMap<(String, String), Option<String>>, AppError> {
    let series = fetcher.query_range(&spec.metric, start, end, step).await?;
    // (namespace, pod) → (该 series 最大点时间戳, 模型名 Option)
    let mut best: HashMap<(String, String), (DateTime<Utc>, Option<String>)> = HashMap::new();
    for s in series {
        let ns = s.labels.get("namespace").cloned().unwrap_or_default();
        let pod = s
            .labels
            .get("pod")
            .cloned()
            .or_else(|| s.labels.get("pod_name").cloned())
            .unwrap_or_default();
        if ns.is_empty() || pod.is_empty() {
            continue;
        }
        let Some(max_ts) = s.points.iter().map(|(ts, _)| *ts).max() else {
            continue;
        };
        let model = s.labels.get(&spec.model_label).cloned();
        let model = if model.as_deref() == Some(UNKNOWN_MODEL_SENTINEL) {
            None
        } else {
            model
        };
        let key = (ns, pod);
        if best.get(&key).map_or(true, |(prev_ts, _)| max_ts > *prev_ts) {
            best.insert(key, (max_ts, model));
        }
    }
    Ok(best.into_iter().map(|(k, (_, m))| (k, m)).collect())
}

/// 按规则填充每条记录的 `inference_model` 字段（标签优先，否则从 pod 名推导）。
///
/// 即使映射为空（指标未启用/查询失败），也应调用本函数——推导不依赖指标。
pub fn apply_model_info(
    records: &mut [CardRecord],
    map: &HashMap<(String, String), Option<String>>,
) {
    for rec in records {
        let key = (rec.namespace.clone(), rec.pod.clone());
        rec.inference_model = match map.get(&key) {
            Some(Some(model)) => model.clone(),
            _ => derive_model_name(&rec.pod),
        };
    }
}
```

`src/pipeline.rs` 顶部 use 加 `use crate::devices::ModelInfoSpec;`（合并进现有 devices use 行）。

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test --lib pipeline`
Expected: PASS——7 个新测试全绿，既有 pipeline 测试不受影响。

- [ ] **Step 5: 提交**

```bash
git add src/pipeline.rs
git commit -m "feat: inference_model_info 映射构建与模型名推导填充逻辑 v1.10.0"
```

---

### Task 3: main.rs 编排（每源一次查询 + 填充）

**Files:**
- Modify: `src/main.rs:165-196`（源循环内）

**Interfaces:**
- Consumes: `pipeline::collect_model_info`（Task 2）、`pipeline::apply_model_info`（Task 2）、`src.model_info`
- Produces: 无新接口；`outcome.records` 在 extend 进全局 `records` 前已被填充 `inference_model`

- [ ] **Step 1: 实现编排**

`src/main.rs` 源循环（第 165 行 `for src in &cfg.sources {` 之后、`for dt_key` 之前）插入：

```rust
        // 模型信息映射：每个源查询一次（可选；失败 → Warning + 空映射，全部走推导兜底）
        let model_map: HashMap<(String, String), Option<String>> =
            if let Some(mi) = &src.model_info {
                if mi.enabled {
                    match pipeline::collect_model_info(&fetcher, mi, start, end, step).await {
                        Ok(map) => map,
                        Err(e) => {
                            warn!("{e}");
                            warnings.push(format!("{e}"));
                            HashMap::new()
                        }
                    }
                } else {
                    HashMap::new()
                }
            } else {
                HashMap::new()
            };
```

把 `let outcome = pipeline::collect_device(...).await;` 改为 `let mut outcome = ...`，并在 warnings 收集之后、`records.extend` 之前插入：

```rust
            // 模型名称填充（映射为空时仍调用，走 pod 名推导兜底）
            pipeline::apply_model_info(&mut outcome.records, &model_map);
```

确认 `src/main.rs` 顶部已有 `use std::collections::HashMap;`（mapping_values 已用，无需新增）。

- [ ] **Step 2: 验证编译与全量测试**

Run: `cargo test`
Expected: 全部测试通过（无 main.rs 相关单测，验证编译 + 既有测试回归）。

- [ ] **Step 3: 提交**

```bash
git add src/main.rs
git commit -m "feat: main 编排每源查询一次模型映射并填充记录 v1.10.0"
```

---

### Task 4: 报表列（mapper 基础列 + reporter 渲染）

**Files:**
- Modify: `src/mapper.rs:35`（`CORE_BASE_COLUMNS`）、`src/mapper.rs:65`（`CORE_BASE_LOCAL_NAMES`）、tests 模块
- Modify: `src/reporter.rs`（`cell_value` 加分支）、tests 模块

**Interfaces:**
- Consumes: `CardRecord.inference_model`（Task 1）
- Produces: 报表基础列「模型名称」（显示名）/ `inference_model`（本地字段名），与 `CORE_BASE_COLUMNS`/`CORE_BASE_LOCAL_NAMES` 一一对应——`build_base_columns`/`build_base_local_names`/`local_name_for_column` 自动生效，`cell_value_for_db` 经 `cell_value` 自动生效（DB 可配 `local_name: "inference_model"`，类型自动落 VARCHAR）。

- [ ] **Step 1: 写失败测试**

`src/mapper.rs` tests 模块追加：

```rust
    #[test]
    fn model_name_column_follows_container_column() {
        let cols = build_base_columns(ColumnFlags::default());
        let container_idx = cols.iter().position(|c| c == "容器名称").unwrap();
        assert_eq!(
            cols.get(container_idx + 1).map(String::as_str),
            Some("模型名称"),
            "「模型名称」应在「容器名称」之后"
        );
        let names = build_base_local_names(ColumnFlags::default());
        let local_idx = names.iter().position(|n| n == "container").unwrap();
        assert_eq!(
            names.get(local_idx + 1).map(String::as_str),
            Some("inference_model"),
            "本地字段名 inference_model 应在 container 之后"
        );
    }
```

`src/reporter.rs` tests 模块追加：

```rust
    #[test]
    fn cell_value_reads_inference_model() {
        use crate::processor::CardRecord;
        let mut rec: CardRecord = Default::default();
        rec.inference_model = "qwen3-8b-mss".into();
        let mapping_borrowed: HashMap<(usize, &str), &str> = HashMap::new();
        let tz: chrono_tz::Tz = "Asia/Shanghai".parse().unwrap();
        assert_eq!(
            cell_value_for_db(&rec, "模型名称", &mapping_borrowed, 0, tz),
            Some("qwen3-8b-mss".into()),
            "模型名称列应输出推理模型名"
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --lib model_name_column_follows_container_column cell_value_reads_inference_model`
Expected: FAIL——「模型名称」不在基础列 / `cell_value` 未命中（`cell_value_for_db` 返回 None 或空）。

- [ ] **Step 3: 实现**

`src/mapper.rs` `CORE_BASE_COLUMNS`（"容器名称" 后）加 `"模型名称",`；
`CORE_BASE_LOCAL_NAMES`（`"container",` 后）加 `"inference_model",`。

`src/reporter.rs` `cell_value`（"容器名称" 分支后）加：

```rust
        "模型名称" => CellValue::Text(rec.inference_model.clone()),
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test`
Expected: 全绿——含既有 `CORE_BASE_COLUMNS`/`LOCAL_NAMES` 长度一致性测试与 e2e 渲染测试（e2e 用 `BASE_COLUMNS.len()` 动态断言列数，自动适配新列）。

- [ ] **Step 5: 提交**

```bash
git add src/mapper.rs src/reporter.rs
git commit -m "feat: 报表新增「模型名称」基础列（容器名称之后） v1.10.0"
```

---

### Task 5: 默认配置模板注释 + README + 版本号

**Files:**
- Modify: `src/config.rs`（`default_config_yaml` 的 sources 注释区，第 289-306 行附近）
- Modify: `README.md:160`（报表列清单）
- Modify: `Cargo.toml:3`（version）

- [ ] **Step 1: 更新默认配置模板注释**

`src/config.rs` 的 `default_config_yaml()` 中 sources 注释块（`#   device_types — 该源覆盖的设备类型列表…` 之后）追加：

```rust
    //   model_info    — 模型名称采集配置（可选）。查询模型信息指标构建
    //                   (namespace, pod) → 模型名映射，填入「模型名称」列。
    //                   enabled     — 是否启用（默认 true）。false 仅跳过指标查询，
    //                                 报表列仍显示从 pod 名推导的名称
    //                   metric      — 模型信息指标名（默认 "inference_model_info"）
    //                   model_label — 模型名所在标签名（默认 "inference_model"）
```

并在示例 sources 块（`device_types: ["nvidia_a10", "ascend_910b"]` 之后）加注释行
`    # model_info:                       # 可选；不配 = 跳过指标查询`（保持示例为纯注释，
不改变实际默认 YAML 结构——`default_yaml_round_trips` 测试依赖默认模板可解析且 `model_info` 为 None）。

- [ ] **Step 2: 更新 README 报表列清单**

`README.md` 第 160 行基础列顺序字符串中，「容器名称」之后插入「模型名称」：

```text
… | Pod | 容器名称 | 模型名称 | 取值时间范围 | …
```

- [ ] **Step 3: 更新版本号**

`Cargo.toml`：`version = "1.9.9"` → `version = "1.10.0"`（MINOR：新增功能）。

- [ ] **Step 4: 验证**

Run: `cargo test`（含 `default_yaml_round_trips`）+ `cargo clippy --all-targets`
Expected: 全绿、零 warning。

- [ ] **Step 5: 提交**

```bash
git add src/config.rs README.md Cargo.toml
git commit -m "docs: 默认配置注释、README 列清单与版本号 1.10.0"
```

---

### Task 6: 最终验证与推送

**Files:** 无

- [ ] **Step 1: 全量验证**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: 全部测试通过（原 96 + 新增 12 个左右），clippy 零 warning。

- [ ] **Step 2: 确认功能逻辑手工核对**

核对 spec §2 三条规则在 `apply_model_info`/`derive_model_name` 中的落点：
1. 标签非 unknown → 用标签值（`Some(Some(model))` 分支）
2. unknown/未匹配 → `derive_model_name`（前 3 段）
3. 不足 3 段 → `"未知"`

- [ ] **Step 3: 推送**

```bash
git push
```

Expected: 远程 `main` 与本地一致（项目 CLAUDE.md 推送规则）。
