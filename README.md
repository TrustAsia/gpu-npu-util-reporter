# gpu-npu-util-reporter

> 从多个 Prometheus 数据源提取 GPU / NPU 计算卡的利用率与显存占用，聚合后生成带**阈值染色**的 Excel 报表。

[![Rust](https://img.shields.io/badge/Rust-1.94%2B-orange)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT-blue)](#license)

## 简介

`gpu-npu-util-reporter` 是一个 **Rust 编写的单次运行 CLI 工具**（可被 cron 包裹做定时任务），面向运维 / SRE 场景：把分散在多个 Prometheus 实例中的计算卡利用率时序数据，按用户指定的时间范围聚合（均值 / 峰值 / 峰值出现时间），关联容器 / Pod / Namespace 归属，可选地关联外部资产表（机房、负责人等），最终输出排版精美、可一眼定位过载 / 闲置卡片的 `.xlsx` 报表。

开箱即支持两类主流计算卡与对应 exporter：

| 设备类型 | 数据源 | 核心利用率指标 | 显存占用策略 |
|----------|--------|----------------|--------------|
| **NVIDIA A10** | DCGM Exporter | `DCGM_FI_DEV_GPU_UTIL` | `FB_USED / (FB_USED + FB_FREE) × 100`（PromQL 组合） |
| **Ascend 910B** | NPU Exporter | `npu_chip_info_utilization` | 优先 `npu_chip_info_hbm_utilization`，为空时 fallback 到 `hbm_used_memory / hbm_total_memory × 100` |

设备类型以**可扩展的"指标配方"**形式配置（见 [`config.yaml`](#配置)），新增设备类型无需改代码。

## 核心特性

- **多源并发采集**：多个 Prometheus 源并行查询，单个源 / 单卡失败降级为 N/A，不中断整体。
- **时间范围聚合**：均值、峰值、峰值出现时间；并列峰值取最早时间戳（稳定）。
- **资产映射**：加载外部 CSV / Excel 资产表，按 `host_ip` / `card_id` 等键 join，把机房位置、负责人等字段**注入到指定基础列的前 / 后**（锚点必须为基础列）。
- **阈值染色（PRD §2.6）**：8 个可独立启用的触发器（核心 / 显存 × 均值 / 峰值 × 高于 / 低于），命中单元格按触发器各自配置的 **HEX 颜色**填充，便于一眼定位过载（红）/ 闲置（橙）。默认全部关闭。
- **健壮的中文错误提示**：致命错误打印中文上下文并退出码 1；非致命警告收集后正常出报表、对应单元格 N/A、退出码 0；全程不 panic。
- **结构化日志**：基于 `tracing`，控制台与文件日志可分别指定级别（trace/debug/info/warn/error），文件日志可独立开关，路径支持模板变量。
- **相对时间**：`time_range` 支持 `now`、`now-7d`、`start+3h` 等相对时间表达式，便于 cron 定时任务配置。
- **路径模板**：输出路径和日志路径支持 `{{start}}`、`{{end}}`、`{{now}}` 等模板变量，自动替换为绝对时间。
- **高内聚低耦合**：模块单向数据流 `config → fetcher → processor → mapper → highlight → reporter`，`MetricFetcher` trait 解耦数据源，`DeviceSpec` 数据化设备规则。

## 快速开始

### 构建

```bash
cargo build --release
# 产物：target/release/gpu-npu-util-reporter
```

### 首次运行（生成默认配置）

```bash
./gpu-npu-util-reporter --config ./config.yaml
# 输出：[提示] 未发现配置文件，已在 ./config.yaml 生成默认配置，请编辑后重新运行。
```

编辑 `config.yaml` 填入你的 Prometheus 地址、时间范围、可选的资产表与阈值触发器，再次运行即生成 `utilization-report.xlsx`。

### CLI 参数

```bash
gpu-npu-util-reporter [--config <path>] [--start "YYYY-MM-DD HH:MM:SS"] [--end "..."] [--output <path>]
```

- `--config`：配置文件路径，默认 `./config.yaml`（不存在则生成默认）。
- `--start` / `--end`：覆盖时间范围，**必须同时给或同时不给**。支持绝对时间（`YYYY-MM-DD HH:MM:SS`）和相对时间表达式（见下文）。
- `--output`：覆盖输出 xlsx 路径（支持模板变量）。

### 相对时间表达式

`--start` / `--end` 和配置文件中的 `time_range.start` / `time_range.end` 均支持相对时间表达式：

```text
语法：<锚点> [ (+|-) <数字><单位> ]
锚点：now | start | end
单位：y(年) | M(月) | d(天) | h(时) | m(分) | s(秒)
```

示例：
- `now` — 当前时刻
- `now-7d` — 7 天前
- `end-7d` — 查询结束时间前 7 天
- `start+3h` — 查询开始时间后 3 小时
- `now-1d12h` — 1 天 12 小时前（复合偏移）

### 路径模板

`report.output_path` 和 `log.file_path` 支持以下模板变量，运行时替换为实际值：

| 变量 | 替换结果 | 示例 |
|------|----------|------|
| `{{start}}` | 起始时间（完整） | `2026-06-18_00-00-00` |
| `{{start_date}}` | 起始日期 | `2026-06-18` |
| `{{start_time}}` | 起始时刻 | `00-00-00` |
| `{{end}}` | 结束时间（完整） | `2026-06-19_00-00-00` |
| `{{end_date}}` | 结束日期 | `2026-06-19` |
| `{{end_time}}` | 结束时刻 | `00-00-00` |
| `{{now}}` | 运行时刻（完整） | `2026-06-20_14-30-00` |
| `{{now_date}}` | 运行日期 | `2026-06-20` |
| `{{now_time}}` | 运行时刻 | `14-30-00` |

## 配置

`config.yaml` 的关键字段（完整带中文注释的模板由首次运行自动生成）：

```yaml
time_range:
  start: "now-7d"          # 支持相对时间：now/start/end [+- N单位(y/M/d/h/m/s)]
  end:   "now"              # 或绝对时间："2026-06-19 00:00:00"

sources:
  - name: "prod-cluster"
    url: "http://192.168.1.100:9090"
    timeout_secs: 30
    device_types: ["nvidia_a10", "ascend_910b"]

devices:
  nvidia_a10:    # 见上方表格；memory 用 composite_ratio
  ascend_910b:   # memory 用 direct_metric + composite_from_total fallback

ownership:
  mode: "last_in_range"   # 或 "instant"

mapping:
  enabled: false
  source_path: "./assets.csv"
  match_keys: ["host_ip", "card_id"]
  columns:
    - source_field: "机房位置"
      rename: "机房"
      position: { direction: after, anchor: "主机IP" }   # 锚点必须为基础列

# 8 个阈值触发器，默认全为 null（关闭）。启用示例：
#   core_avg_above:
#     enabled: true
#     threshold: 80
#     color: "#FF0000"   # 高于 80% 染红（过载）
thresholds:
  core_avg_above:  null
  core_avg_below:  null
  core_peak_above: null
  core_peak_below: null
  mem_avg_above:   null
  mem_avg_below:   null
  mem_peak_above:  null
  mem_peak_below:  null

# 日志配置
log:
  console_level: "info"    # 控制台日志级别：trace/debug/info/warn/error
  file_enabled: false      # 是否启用文件日志
  file_level: "debug"      # 文件日志级别
  file_path: "./logs/{{now}}.log"   # 支持模板变量

# 报表输出（output_path 支持模板变量）
report:
  output_path: "./utilization-report.xlsx"
  query_step_secs: 60
```

## 报表列

基础列顺序固定，资产映射列按配置插入到锚点列前 / 后：

`数据来源 | 主机IP | [映射列...] | 节点名称 | 计算卡编号 | 设备类型 | Namespace | Pod | 容器名称 | 取值时间范围 | 核心利用率平均值 | 核心利用率峰值 | 核心利用率峰值出现时间 | 显存占用率平均值 | 显存占用率峰值 | 显存占用率峰值出现时间`

- 首行冻结 + 加粗 + 深蓝底白字。
- 利用率列百分比格式（`0.00%`），N/A 单元格写字符串 `N/A`（不参与染色）。
- 时间列以 `YYYY-MM-DD HH:MM:SS` 文本输出。
- 列宽自适应（clamp 到 [10, 50]）。

## 架构

```
src/
├── main.rs        # CLI 入口与编排
├── lib.rs         # 库入口（pub 各模块，供集成测试复用）
├── config.rs      # YAML 解析、默认配置生成、CLI 覆盖
├── devices.rs     # 设备指标配方 DeviceSpec + A10/910B 预设
├── fetcher.rs     # MetricFetcher trait + Prometheus HTTP 实现
├── logging.rs     # 日志初始化（tracing，控制台+文件独立级别）
├── pipeline.rs    # 采集编排、分组、归属取值
├── processor.rs   # 时序聚合、HBM fallback
├── mapper.rs      # 资产表加载、@key join、列位置排布
├── highlight.rs   # 8 触发器阈值染色规则 + HEX 校验
├── reporter.rs    # Excel 渲染（样式 + 染色集成）
├── template.rs    # 路径模板引擎（{{start}}, {{end}}, {{now}} 等）
├── time_expr.rs   # 相对时间表达式解析（now-7d, start+3h 等）
└── error.rs       # 统一错误类型 + 中文友好提示
```

依赖方向单向无环：`config ← main → fetcher → processor → mapper → highlight → reporter`。

## 开发

```bash
cargo test          # 91 个测试（90 单元 + 1 端到端渲染回读）
cargo clippy        # 零 warning
```

测试覆盖：聚合算法边界、HBM fallback、8 触发器各方向命中 / 跳过 / 同列取首、HEX 校验、列位置排布、Join 命中 / 未命中、默认配置 round-trip、端到端 xlsx 渲染回读。

### 设计文档

- 产品需求：[`PRD.md`](./PRD.md)
- 设计与决策：[`docs/superpowers/specs/`](./docs/superpowers/specs/)
- 实现计划：[`docs/superpowers/plans/`](./docs/superpowers/plans/)

## License

MIT
