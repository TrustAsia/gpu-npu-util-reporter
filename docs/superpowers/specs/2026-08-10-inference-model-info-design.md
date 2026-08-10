# 「模型名称」列 + inference_model_info 采集

**日期**: 2026-08-10
**版本**: v1.10.0 (MINOR — 新功能)

## 背景

报表按 (`host_ip`, `card_id`) 一行一卡，已有归属信息（namespace/pod/container），
但缺少"这张卡在跑什么模型"的信息。集群中有 `model-info-exporter` 作业暴露
`inference_model_info` 指标（值为 1 的 gauge，标签携带 pod 的推理模型名），
可用于从 pod 匹配模型名称。

指标实测结构（Kuboard 代理测试通道验证）：

```json
{
  "metric": {
    "__name__": "inference_model_info",
    "inference_model": "asr-gx",
    "pod": "whisper-large-v3-turbo-v2-5d88994856-wh6ml",
    "pod_name": "whisper-large-v3-turbo-v2-5d88994856-wh6ml",
    "namespace": "ns-18877"
  },
  "value": [1786362954.292, "1"]
}
```

要点：
- 模型名在 `inference_model` 标签；无模型信息的 pod 该值为 `"unknown"`
- `pod` 与 `pod_name` 两个标签同时存在且值相同（兼容 DCGM 与 NPU exporter 的不同命名）
- 注：Kuboard 代理 + Cookie 认证仅为开发期测试通道，**工具不新增任何认证能力**，
  生产仍直连各数据源配置的 Prometheus 地址

## 设计

### 1. 报表列

在基础列「容器名称」之后插入「模型名称」：

`… | Pod | 容器名称 | 模型名称 | 取值时间范围 | …`

- 该列**始终出现**（核心基础列），值为文本
- 本地字段名 `inference_model`（可用于 database.columns 映射；默认模板不新增该列，
  避免已有 DB 表被判定缺列而中断——用户按需在配置中启用）

### 2. 匹配与取值规则（每行记录）

数据源级查询一次 `inference_model_info`（query_range 覆盖整个时间窗，step 同现有配置），
构建 `(namespace, pod) → 模型名` 映射：

- 同一 pod 出现多条 series（标签集不同）时，取**时间段内有数据的最后一条**
  （按该 series 最大点时间戳比较，取最大者）——与 `last_in_range` 归属语义一致
- 指标同时带 `pod` 与 `pod_name` 标签（值相同），取 pod 优先、pod_name
  兜底，兼容 DCGM 与 NPU exporter 的不同标签名
- `inference_model` 值为 `"unknown"` 的 series 以 `None` 存入映射（与查不到等价）

每行记录取值顺序：

1. 记录 `(namespace, pod)` 在映射中且值为非 `unknown` → 直接用标签值
   （如 `asr-gx`、`qwen3-8b-mss`）
2. 值为 `unknown` 或 pod 不在映射中 → 从记录 pod 名推导：
   按 `-` 分隔取前 3 段（即保留到第 2 个连字符为止）
   - `tele-tts-onnx-hanyu-v20260604-54d775747-26hhf` → `tele-tts-onnx`
   - `qwen3-8b-w8a8-jifei-78b98cc54-dpvkc` → `qwen3-8b-w8a8`
3. pod 名为空或不足 3 段 → `"未知"`

### 3. 配置（源级可选块）

```yaml
sources:
  - name: "prod-cluster"
    url: "http://..."
    timeout_secs: 30
    device_types: ["nvidia_a10", "ascend_910b"]
    model_info:                    # 新增，可选；不配 = 跳过指标查询
      enabled: true                # 默认 true
      metric: "inference_model_info"   # 默认
      model_label: "inference_model"   # 默认
```

- `ModelInfoSpec` 三个字段全部带默认值；`SourceConfig.model_info` 为
  `Option` + `#[serde(default)]`，旧配置文件不受影响
- `enabled: false` 仅跳过指标查询；「模型名称」列仍显示从 pod 名推导的名称
  （推导不依赖指标）
- 校验：`metric` 必须是合法 Prometheus 指标名，`model_label` 必须是合法标签名
  （复用现有校验函数）；查询失败 → Warning + 空映射，全部走推导兜底

### 4. 代码改动

| 文件 | 变更类型 | 说明 |
|------|----------|------|
| `src/devices.rs` | 修改 | 新增 `ModelInfoSpec { enabled, metric, model_label }`（含默认值） |
| `src/config.rs` | 修改 | `SourceConfig` 加 `model_info: Option<ModelInfoSpec>`，校验，默认配置模板注释 |
| `src/processor.rs` | 修改 | `CardRecord` 加 `inference_model: String` |
| `src/pipeline.rs` | 修改 | 新增 `collect_model_info()`（查询+构建映射，含 (ns,pod)/(ns,pod_name) 双键）、`apply_model_info()`（按规则填充）、`derive_model_name()`（取前 3 段） |
| `src/main.rs` | 修改 | 每个源建完 fetcher 后先查一次模型映射，再对每批 `outcome.records` 填充；不修改 `collect_device` 签名 |
| `src/mapper.rs` | 修改 | `CORE_BASE_COLUMNS`/`CORE_BASE_LOCAL_NAMES` 在「容器名称」后插入 |
| `src/reporter.rs` | 修改 | `cell_value` + `cell_value_for_db` 加「模型名称」分支 |
| `README.md` | 修改 | 报表列清单更新 |
| `Cargo.toml` | 修改 | 版本号 → 1.10.0 |

`db.rs` 无需改：`inference_model` 按 local_name 规则自动落入 VARCHAR(255)。

### 5. 测试要点

1. `derive_model_name` 单元测试：
   - `tele-tts-onnx-hanyu-v20260604-54d775747-26hhf` → `tele-tts-onnx`
   - `a-b-c`（恰 3 段）→ `a-b-c`；`a-b` → `未知`；空串 → `未知`
2. `apply_model_info`：标签优先 / unknown 推导 / 查不到推导 / 推导失败写"未知" 四分支
3. `collect_model_info`：MockFetcher 验证双键插入、同 pod 多 series 取最后一条
4. 回归：`CORE_BASE_COLUMNS` 与 `LOCAL_NAMES` 长度一致性已有测试自动覆盖；
   e2e 渲染测试的 `CardRecord` 字面量补新字段
5. 验证命令：`cargo test` 全绿 + `cargo clippy` 零 warning

## 变更文件清单

| 文件 | 变更类型 |
|------|----------|
| `src/devices.rs` | 修改 |
| `src/config.rs` | 修改 |
| `src/processor.rs` | 修改 |
| `src/pipeline.rs` | 修改 |
| `src/main.rs` | 修改 |
| `src/mapper.rs` | 修改 |
| `src/reporter.rs` | 修改 |
| `README.md` | 修改 |
| `Cargo.toml` | 修改（版本 1.10.0） |
