# 设计文档：GPU/NPU 多源利用率监控与报表生成系统

- 日期：2026-06-19
- 语言：Rust (Edition 2021)
- 状态：待评审

## 1. 目标与范围

从多个 Prometheus 数据源中，提取用户指定时间范围内 GPU（NVIDIA A10 / DCGM Exporter）和 NPU（Ascend 910B / NPU Exporter）计算卡的利用率与显存占用情况，关联容器/Pod/Namespace 归属，可选地关联外部资产表，最终输出排版精美的 `.xlsx` 报表。

本系统是**单次运行的 CLI 工具**（可被 cron 包裹做定时任务），不是常驻服务。

### 范围内
- 多 Prometheus 源并行查询
- 时间范围内聚合（均值、峰值、峰值时间）
- 标签归属取值（瞬时 / 末态）
- 外部资产表映射（CSV/Excel）+ 动态列插入
- Excel 报表渲染（冻结首行、百分比、列宽）

### 范围外（YAGNI）
- 不做 Web UI / HTTP 服务
- 不做实时流式监控
- 不做跨数据源去重（按数据源分行输出）
- 不做告警 / 阈值触发

## 2. 关键决策（已与用户确认）

| # | 决策点 | 选择 |
|---|--------|------|
| D1 | 运行方式 | CLI 参数优先 + 配置兜底。`--start`/`--end`/`--config`/`--output` 覆盖 `config.yaml` 默认值 |
| D2 | 主机 IP 来源 | 可配置标签优先，`instance` 标签去端口兜底 |
| D3 | device_type 语义 | 可扩展的"指标配方"——每个设备类型是一组 (核心利用率指标, 显存查询, 卡编号标签) |
| D4 | 跨源去重 | 不去重，按数据源分行 |
| D5 | 映射列位置 | 锚点列 + before/after 方向 |
| D6 | 阈值染色粒度 | 只染命中触发器的单个单元格；每个触发器独立配 HEX 颜色；默认全关 |

## 3. 总体架构

遵循 PRD §5.1 的模块划分（在原五模块基础上新增独立的阈值染色模块）。每个模块单一职责，通过 trait / struct 边界解耦。

```
src/
├── main.rs              # CLI 入口：解析参数 → 编排各模块 → 汇总错误退出码
├── config.rs            # config：YAML 反序列化、默认配置生成、CLI 合并
├── fetcher.rs           # 数据源适配层：MetricFetcher trait + Prometheus HTTP 客户端
├── devices.rs           # 设备类型"指标配方"：DeviceSpec 定义 + DCGM/NPU 预设
├── processor.rs         # 数据处理：时间范围聚合、HBM fallback、归属取值
├── mapper.rs            # 资产映射：加载资产表 + Join + 列位置排布
├── highlight.rs         # 阈值染色：8 个触发器规则 + 命中判断 + 颜色解析
├── reporter.rs          # 报告渲染：rust_xlsxwriter 封装，样式与排版（含染色）
└── error.rs             # 统一错误类型 + 中文友好提示
```

### 数据流（单向流水线，无环）

```
config.yaml + CLI args
        │
        ▼
   [config]  ──► AppConfig（含时间范围、数据源、设备配方、映射、报表列）
        │
        ▼
   [fetcher]  ──► 对每个 (数据源 × 设备类型) 并发发 PromQL 查询
        │         失败的卡片/源 → 记 Warning，对应行留 N/A
        ▼
   [processor] ──► 把每条时序聚合成一张卡的统计（均值/峰值/峰值时间）
        │         + 归属取值（瞬时/末态）+ HBM fallback
        ▼
   [mapper]   ──► 按锚点把资产字段注入到每行（可选，开关控制）
        │
        ▼
   [reporter] ──► 写 .xlsx（冻结首行、百分比格式、列宽自适应）
```

## 4. 模块详细设计

### 4.1 config 模块

**职责**：YAML 反序列化、生成带中文注释的默认配置、CLI 参数合并。

```rust
/// 应用顶层配置
struct AppConfig {
    time_range: TimeRangeConfig,        // start/end 时间，格式见 TimeFormat
    sources: Vec<SourceConfig>,         // 多个 Prometheus 源
    devices: DeviceTypesConfig,         // 设备类型指标配方（含预设）
    ownership: OwnershipConfig,         // 归属取值模式
    mapping: Option<MappingConfig>,     // None = 关闭资产映射
    thresholds: ThresholdsConfig,       // 阈值染色触发器（8 个，默认全关）
    report: ReportConfig,               // 输出路径、列顺序、样式开关
}

/// 单个 Prometheus 数据源
struct SourceConfig {
    name: String,           // 别名，写入"数据来源"列
    url: String,            // http://host:port
    timeout_secs: u64,
    device_types: Vec<String>, // 该源要采集哪些设备类型（引用 devices 里的 key）
}

/// 时间范围
struct TimeRangeConfig {
    start: String,   // "2026-06-01 00:00:00"
    end: String,
}
```

- **解析**：`serde` + `serde_yaml`。时间字符串统一 `YYYY-MM-DD HH:MM:SS`，用 `chrono` 的 `NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S")`。
- **开箱即用**：启动时若 `--config` 指定路径不存在，调用 `generate_default_config()` 写一份带中文注释、含 NVIDIA A10 与 Ascend 910B 两套预设的 `config.yaml`，然后正常退出并提示用户编辑。
- **CLI 合并**：`clap` 解析 `--start`/`--end`/`--config`/`--output`；非空则覆盖配置里对应字段。时间参数若只给了一个，报错（要求 start+end 同时给或不给）。

### 4.2 devices 模块（设备类型指标配方）

**职责**：把 PRD §2.2 的两种设备抽取规则抽象成数据，可扩展。这是 D3 决策的落点。

```rust
/// 一个设备类型的"指标配方"——告诉 fetcher 该查什么、processor 该算什么
struct DeviceSpec {
    /// 显示名，如 "NVIDIA A10"，写入"设备类型"列
    display_name: String,
    /// 核心利用率指标名，如 "DCGM_FI_DEV_GPU_UTIL" / "npu_chip_info_utilization"
    core_util_metric: String,
    /// 显存查询策略：直接指标 或 组合公式
    memory: MemoryStrategy,
    /// 卡编号所在的标签名，如 "gpu" / "id"
    card_id_label: String,
}

enum MemoryStrategy {
    /// 直接读一个利用率指标，如 NPU 的 npu_chip_info_hbm_utilization
    DirectMetric(DirectMetricBody),
    /// 用 used/(used+free)*100 组合，如 GPU 的 FB_USED/(FB_USED+FB_FREE)
    CompositeRatio(CompositeRatioBody),
    /// 用 used/total*100 组合，如 NPU fallback 的 hbm_used_memory / hbm_total_memory
    CompositeFromTotal(CompositeFromTotalBody),
}

// 各变体用 newtype 包装体承载字段；serde 用 #[serde(untagged)]
// 使 YAML 形如 direct_metric: {...} / composite_ratio: {...} / composite_from_total: {...}
// （serde_yaml 不支持默认 externally-tagged 的字段变体，untagged+newtype 是唯一干净方案）
```

**预设**（写在 config 默认值 + 代码里兜底）：
- `nvidia_a10` → core=`DCGM_FI_DEV_GPU_UTIL`, memory=`CompositeRatio{composite_ratio{used=DCGM_FI_DEV_FB_USED, free=DCGM_FI_DEV_FB_FREE}}`, card_id=`gpu`
- `ascend_910b` → core=`npu_chip_info_utilization`, memory=`DirectMetric{direct_metric{metric=npu_chip_info_hbm_utilization, fallback=CompositeFromTotal{composite_from_total{used=npu_chip_info_hbm_used_memory, total=npu_chip_info_hbm_total_memory}}}}`, card_id=`id`

> 注：NPU fallback 用 `used/total*100`（PRD §2.2 明确 `npu_chip_info_hbm_used_memory / npu_chip_info_hbm_total_memory`）。`CompositeRatio`（used/free）用于 GPU，`CompositeFromTotal`（used/total）用于 NPU fallback，两个变体分别匹配各自公式，processor 不做猜测。

### 4.3 fetcher 模块（数据源适配层）

**职责**：抽象 PromQL 拼装与 HTTP 调用，解耦具体 exporter。

```rust
#[async_trait]
trait MetricFetcher {
    /// 查询某指标在 [start,end] 范围内的 range vector（带标签的时序点）
    /// 返回 Vec<Series>，每个 Series 是 (labels, Vec<(timestamp, value)>)
    async fn query_range(
        &self,
        promql: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        step: Duration,
    ) -> Result<Vec<Series>, FetchError>;
}

struct Series {
    labels: HashMap<String, String>,
    points: Vec<(DateTime<Utc>, f64)>,
}

/// 具体实现：调用 Prometheus /api/v1/query_range
struct PrometheusFetcher { client: reqwest::Client, base_url: String, timeout: Duration }
```

- 用 `reqwest` + `tokio`。Prometheus 返回 JSON，用 `serde` 反序列化 `data.result[].value[]` 与 `metric`。
- `step` 由 config 给（默认 60s）。查询失败的指标返回 `FetchError`，由调用方决定降级（见 4.4）。
- PromQL 拼装放这里：把 `DeviceSpec` 转成查询表达式。GPU 显存直接用一条 PromQL 组合表达式 `DCGM_FI_DEV_FB_USED / (DCGM_FI_DEV_FB_USED + DCGM_FI_DEV_FB_FREE) * 100`（Prometheus 原生向量除法，一条 range query 拿到结果）。NPU 显存不能在 PromQL 里做 fallback（fallback 取决于 direct 是否为空），所以 fetcher 对 NPU 一次性把 direct + fallback 两路指标都拉回来，由 processor 决定用哪一路。
- **并发**：对所有 `(source, device_type, metric)` 用 `tokio::join` / `futures::join_all` 并发查询。单个失败不阻断整体。

### 4.4 processor 模块（数据处理与聚合）

**职责**：把原始 `Series` 聚合成"每张卡的统计行"，执行 HBM fallback 与归属取值。

```rust
/// 一张卡在时间范围内的最终统计结果（一行报表的数据来源）
struct CardRecord {
    source_name: String,
    host_ip: String,
    node_name: String,        // 来自标签，可选
    card_id: String,
    device_type: String,
    // 归属
    namespace: String,
    pod: String,
    container: String,
    // 核心利用率
    core_avg: Option<f64>,        // None = N/A
    core_peak: Option<f64>,
    core_peak_time: Option<DateTime<Utc>>,
    // 显存占用率
    mem_avg: Option<f64>,
    mem_peak: Option<f64>,
    mem_peak_time: Option<DateTime<Utc>>,
    // 时间范围（用于报表"取值时间范围"列）
    range_start: DateTime<Utc>,
    range_end: DateTime<Utc>,
}
```

**聚合算法**：
- 均值 = `points.values().sum() / count`
- 峰值 = `points.max()`；峰值时间 = 该点对应的 timestamp
- 空序列 → 所有统计字段为 `None`（报表显示 N/A）

**HBM fallback**：NPU 显存——先尝试 `DirectMetric`；若该指标查询返回空或全空，processor 取 `fallback` 分支：把已拉到的 `hbm_used_memory` 与 `hbm_total_memory` 两条序列按 (host+card_id) 对齐，逐点算 `used/total*100`，再走正常聚合。两者都失败 → `mem_* = None`。

**归属取值（OwnershipConfig.mode）**：
- `Instant`：单独发一个瞬时查询（`/api/v1/query`，不带 range），取当前 container/pod/namespace 标签。
- `LastInRange`：遍历时间范围内该卡相关的归属标签序列，取最后一个非空值。
- 标签字段名（container/pod/namespace）来自 device spec 或 config，DCGM 与 NPU 的标签名不同（NPU 是 `container_name`/`pod_name`/`namespace`，DCM 通常 `container`/`pod`/`namespace`），在 DeviceSpec 里补一个 `labels: LabelMapping` 字段统一抽象。

### 4.5 mapper 模块（资产映射引擎）

**职责**：独立处理资产表加载、Join、列位置排布。开关关闭时此模块完全跳过。

```rust
struct MappingConfig {
    enabled: bool,
    source_path: String,            // CSV 或 .xlsx
    /// 匹配键：从计算卡记录里取哪些字段拼成 join key
    match_keys: Vec<MatchKey>,      // 如 [HostIp, CardId]
    /// 要注入的列
    columns: Vec<MappingColumn>,
}

struct MappingColumn {
    /// 资产表里的源列名
    source_field: String,
    /// 注入到报表后的新列名
    rename: String,
    /// 插入位置：锚点列名 + 方向
    position: InsertPosition,
}

enum InsertPosition { Before(String), After(String) }
enum MatchKey { HostIp, CardId, NodeName } // 可扩展
```

**列位置排布算法**：
1. reporter 先定义基础列的有序列表 `base_columns`。
2. mapper 把每个 `MappingColumn.position` 解析成目标 index：`Before(X)` → X 的 index；`After(X)` → X 的 index + 1。
3. 按 index 升序插入映射列；同 index 的按 config 顺序稳定排列。
4. 若锚点列名不存在，记 Warning 并把该列追加到末尾。

**Join 逻辑**：把资产表读成 `HashMap<join_key_string, HashMap<field, value>>`，key 由 match_keys 拼成（如 `"10.0.0.1|0"`）。每行 CardRecord 用同样的 key 查；命中则注入，未命中则该列留空 + 可选 Warning。

**资产表读取**：CSV 用 `csv` crate；Excel 用 `calamine`（只读）。自动按扩展名分流。

### 4.6 highlight 模块（阈值染色）

**职责**：独立于 reporter 实现 PRD §2.6 的阈值规则判断与颜色解析。reporter 只负责"把指定颜色涂到指定单元格"，染色规则由本模块计算输出。这样规则演进（如新增触发器）不影响渲染层。

```rust
/// 阈值染色总配置
struct ThresholdsConfig {
    /// 8 个触发器，key 固定为触发器名（见下表）
    /// 用 HashMap<String, TriggerConfig> 或显式 8 字段 struct 均可，推荐显式 struct 保证可发现性
    triggers: ThresholdTriggers,
}

/// 8 个触发器的显式集合（缺省字段 = 该触发器关闭）
struct ThresholdTriggers {
    core_avg_above:  Option<TriggerConfig>,
    core_avg_below:  Option<TriggerConfig>,
    core_peak_above: Option<TriggerConfig>,
    core_peak_below: Option<TriggerConfig>,
    mem_avg_above:   Option<TriggerConfig>,
    mem_avg_below:   Option<TriggerConfig>,
    mem_peak_above:  Option<TriggerConfig>,
    mem_peak_below:  Option<TriggerConfig>,
}

/// 单个触发器：None = 未配置/关闭
struct TriggerConfig {
    enabled: bool,         // false 则该触发器整体跳过（即便配了阈值）
    threshold: f64,        // 0–100 的阈值
    color: HexColor,       // HEX 颜色，如 "#FF0000"
}

/// 包装类型，反序列化时校验 HEX 合法性（#RRGGBB 或 #RGB）
struct HexColor(String);
```

**触发器 → 报表列 → 字段映射**（判断哪个单元格该染色）：

| 触发器 | 对应报表列 | 取 CardRecord 字段 | 判断 |
|--------|-----------|-------------------|------|
| `core_avg_above` | 核心利用率平均值 | `core_avg` | `value > threshold` |
| `core_avg_below` | 核心利用率平均值 | `core_avg` | `value < threshold` |
| `core_peak_above` | 核心利用率峰值 | `core_peak` | `value > threshold` |
| `core_peak_below` | 核心利用率峰值 | `core_peak` | `value < threshold` |
| `mem_avg_above` | 显存占用率平均值 | `mem_avg` | `value > threshold` |
| `mem_avg_below` | 显存占用率平均值 | `mem_avg` | `value < threshold` |
| `mem_peak_above` | 显存占用率峰值 | `mem_peak` | `value > threshold` |
| `mem_peak_below` | 显存占用率峰值 | `mem_peak` | `value < threshold` |

**核心 API**：给定一行 CardRecord，返回"列名 → 颜色"的染色映射。

```rust
impl ThresholdTriggers {
    /// 计算一行数据命中的染色：返回 (报表列名, HEX颜色) 列表
    /// 同一列若被多个触发器命中，按 struct 字段声明顺序取第一个（稳定、可预期）
    fn evaluate_row(&self, record: &CardRecord) -> Vec<(&str, &HexColor)>;
}
```

**判断规则细节**：
- 字段为 `None`（N/A）→ 跳过，不染色、不算命中。
- `enabled: false` → 该触发器跳过。
- 阈值边界用严格大于/小于（`>` / `<`），不包含等于，避免边界值频繁触发。
- HEX 解析在 config 反序列化阶段完成（实现 `serde::Deserialize` for `HexColor`），非法 HEX（如 `"red"`、`"#GGG"`）直接报中文配置错误，不进入运行期。

**与 reporter 的接口**：reporter 遍历每行时调用 `evaluate_row`，拿到 `Vec<(&str, &HexColor)>`，在写该行单元格时若列名命中则套用对应背景色 `Format`。颜色映射成 `rust_xlsxwriter::Color` 一次性缓存（同一 HEX 复用同一 Format 对象，避免重复构造）。

### 4.7 reporter 模块（报告渲染层）

**职责**：隔离 `rust_xlsxwriter`，专职把 `Vec<CardRecord>` + 列布局写成带样式的 `.xlsx`。

- **列顺序**：基础列固定顺序 + mapper 注入的映射列（按计算好的 index 插入）。基础列（PRD §3）：
  `数据来源 | 主机IP | 节点名称 | 计算卡编号 | 设备类型 | Namespace | Pod | 容器名称 | 取值时间范围 | 核心利用率平均值 | 核心利用率峰值 | 核心利用率峰值出现时间 | 显存占用率平均值 | 显存占用率峰值 | 显存占用率峰值出现时间`
- **样式**：
  - 首行冻结 + 加粗 + 背景色（如深蓝底白字）。
  - 利用率列（6 个 *_avg/*_peak）数字格式 `0.00%`（值是 0-100 的百分数，存为 `value/100` 让格式串加 %）。N/A 写成字符串 "N/A"。
  - 时间列格式 `yyyy-mm-dd hh:mm:ss`。
  - 列宽自适应：先按列内容最长长度估算，设上下限（min 10, max 40 字符宽）。
- 报表文件名默认 `utilization-report-{start}-{end}.xlsx`，`--output` 可覆盖。
- **染色集成**：写每行数据单元格时，调用 highlight 模块的 `evaluate_row` 拿到命中列的颜色映射，对相应单元格套用背景色 Format（仅染单元格，不染整行）。颜色 Format 按 HEX 缓存复用。N/A 单元格不参与染色。

### 4.8 error 模块（错误处理）

```rust
#[derive(thiserror::Error)]
enum AppError {
    #[error("[错误] 配置文件 {path} 解析失败：{reason}")]
    Config { path: String, reason: String },
    #[error("[错误] 无法连接到 Prometheus 数据源 {source}（{url}），请检查网络或配置：{detail}")]
    Prometheus { source: String, url: String, detail: String },
    #[error("[错误] PromQL 查询返回异常（{source}）：{detail}")]
    Promql { source: String, detail: String },
    #[error("[错误] 时间格式无效：{raw}，请使用 YYYY-MM-DD HH:MM:SS")]
    TimeFormat { raw: String },
    #[error("[错误] 阈值触发器 {trigger} 的颜色 {raw} 不是合法的 HEX 颜色（需为 #RRGGBB 或 #RGB）")]
    InvalidColor { trigger: String, raw: String },
    #[error("[错误] 资产表加载失败（{path}）：{detail}")]
    Mapping { path: String, detail: String },
    #[error("[错误] 报表写入失败：{detail}")]
    Report { detail: String },
    #[warning("{msg}")] // 非致命，仅打印
    Warning { msg: String },
}
```

- 致命错误（配置错、时间格式错、所有源都失败）→ 打印中文错误 → 进程退出码 1。
- 非致命（单卡/单源/单指标失败）→ 收集为 Warning 列表，正常输出报表，对应单元格 N/A，退出码 0。
- **绝不 panic**：所有 `unwrap`/`expect` 禁用，全部走 `?` + `Result`。

## 5. 配置文件示例（节选，将作为默认生成模板）

```yaml
# === 时间范围（可被 --start/--end 覆盖） ===
time_range:
  start: "2026-06-18 00:00:00"
  end:   "2026-06-19 00:00:00"

# === Prometheus 数据源 ===
sources:
  - name: "prod-cluster"
    url: "http://192.168.1.100:9090"
    timeout_secs: 30
    device_types: ["nvidia_a10", "ascend_910b"]

# === 设备类型指标配方（可自定义新增） ===
devices:
  nvidia_a10:
    display_name: "NVIDIA A10"
    core_util_metric: "DCGM_FI_DEV_GPU_UTIL"
    card_id_label: "gpu"
    labels: { container: "container", pod: "pod", namespace: "namespace" }
    memory:
      composite_ratio: { used: "DCGM_FI_DEV_FB_USED", free: "DCGM_FI_DEV_FB_FREE" }
  ascend_910b:
    display_name: "Ascend 910B"
    core_util_metric: "npu_chip_info_utilization"
    card_id_label: "id"
    labels: { container: "container_name", pod: "pod_name", namespace: "namespace" }
    memory:
      direct_metric: { metric: "npu_chip_info_hbm_utilization",
                fallback: { composite_from_total: { used: "npu_chip_info_hbm_used_memory",
                                                    total: "npu_chip_info_hbm_total_memory" } } }

# === 主机 IP 取值（标签优先，instance 兜底） ===
host_ip:
  prefer_label: "ip"     # 取不到时从 instance 标签去端口

# === 归属取值模式 ===
ownership:
  mode: "last_in_range"  # 或 "instant"

# === 资产映射（enabled: false 则关闭） ===
mapping:
  enabled: false
  source_path: "./assets.csv"
  match_keys: ["host_ip", "card_id"]
  columns:
    - source_field: "机房位置"
      rename: "机房"
      position: { after: "主机IP" }
    - source_field: "负责人"
      rename: "负责人"
      position: { after: "机房" }

# === 阈值染色触发器（8 个，默认全部关闭；enabled: true 才生效） ===
# 阈值为 0–100 的数值（与利用率原始范围一致）；color 必须是 HEX 格式（如 #RRGGBB）
# 仅染命中触发器的单个数值单元格；同一单元格被多个触发器命中时取首个命中
thresholds:
  core_avg_above:           # 核心利用率平均值 高于 阈值 → 染该单元格
    enabled: false
    threshold: 80
    color: "#FF0000"        # 红，警示过载
  core_avg_below:           # 核心利用率平均值 低于 阈值 → 染该单元格
    enabled: false
    threshold: 10
    color: "#FFA500"        # 橙，提示闲置
  core_peak_above:
    enabled: false
    threshold: 95
    color: "#FF0000"
  core_peak_below:
    enabled: false
    threshold: 5
    color: "#FFA500"
  mem_avg_above:
    enabled: false
    threshold: 90
    color: "#FF0000"
  mem_avg_below:
    enabled: false
    threshold: 10
    color: "#FFA500"
  mem_peak_above:
    enabled: false
    threshold: 95
    color: "#FF0000"
  mem_peak_below:
    enabled: false
    threshold: 5
    color: "#FFA500"

# === 报表输出 ===
report:
  output_path: "./utilization-report.xlsx"
  query_step_secs: 60
```

## 6. Cargo 依赖

```toml
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
calamine = "0.24"          # 只读 Excel 资产表
rust_xlsxwriter = "0.71"   # 写报表
futures = "0.3"
```

## 7. 错误提示示例（PRD §5.2 落地）

| 场景 | 提示 |
|------|------|
| 连不上 Prometheus | `[错误] 无法连接到配置的 Prometheus 数据源 prod-cluster（http://192.168.1.100:9090），请检查网络或配置：连接超时` |
| PromQL 返回错误 | `[警告] 数据源 prod-cluster 查询 npu_chip_info_hbm_utilization 失败：bad_data: unknown metric，该指标将走 fallback` |
| 单卡全空 | `[警告] 卡片 10.0.0.1/gpu=2 无有效数据点，报表对应行标记 N/A` |
| 时间格式错 | `[错误] 时间格式无效：2026/06/18 00:00:00，请使用 YYYY-MM-DD HH:MM:SS` |
| 配置缺失 | `[错误] 配置文件 ./config.yaml 解析失败：time_range.start 字段缺失` |
| 阈值颜色非法 | `[错误] 阈值触发器 core_avg_above 的颜色 red 不是合法的 HEX 颜色（需为 #RRGGBB 或 #RGB）` |

## 8. 测试策略

- **processor 单元测试**：喂构造的 `Series`，断言 avg/peak/peak_time 计算正确；空序列返回 None；HBM fallback 在 direct 为空时正确触发。
- **mapper 单元测试**：构造 base_columns + 多个 MappingColumn（含 Before/After/不存在的锚点），断言最终列顺序；Join 命中/未命中分支。
- **highlight 单元测试**：构造 CardRecord，验证 8 个触发器各方向的命中/不命中（含边界值严格 `>`/`<`、enabled:false 跳过、None 字段跳过、同列多触发器取首个）；HexColor 反序列化合法/非法分支（`#RGB`、`#RRGGBB` 通过，`red`、`#GGG`、`#12345` 报错）。
- **devices 单元测试**：预设的 DeviceSpec 字段正确；MemoryStrategy fallback 链可达。
- **config 单元测试**：默认配置能被自身反序列化（round-trip）；CLI 合并覆盖优先级正确；默认 thresholds 全为关闭。
- **reporter 单元测试**：用 `rust_xlsxwriter` 写到 `Vec<u8>`（内存），用 `calamine` 读回断言行数、首行表头、数值列回读为正确浮点值（注：calamine 0.26 稳定 API 不暴露单元格填充色，因此填充色染色不通过读回文件验证，而是由 highlight 模块的 `evaluate_row` 单元测试覆盖命中逻辑，reporter 侧通过断言"对命中行调用了带背景色的 Format 写入"来覆盖）。
- fetcher 做 trait 抽象后可用 mock 实现（不连真实 Prometheus）测编排逻辑。
- 不写需要真实 Prometheus 的集成测试（环境不可得）。

## 9. 非功能要求落地

- **高内聚低耦合**：六模块各自独立，依赖方向单向（config ← main → fetcher → processor → mapper → highlight → reporter），无环。trait 边界：`MetricFetcher` 抽象数据源，`DeviceSpec` 数据化设备规则，highlight 与 reporter 通过"列名→颜色"映射解耦（规则演进不影响渲染）。
- **中文注释**：所有 struct/trait/复杂函数加中文 doc 注释，说明设计意图。config 默认模板全中文注释。
- **健壮性**：`Result` + `thiserror`，无 panic，单点失败降级为 N/A。
- **并发**：fetcher 层 `tokio` 并发查多源多指标，缩短总耗时。
