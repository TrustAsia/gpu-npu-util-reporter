# 主机指标纳入设备类型指标配方

**日期**: 2026-06-26
**版本**: v1.8.0 (MINOR — 新功能，移除旧全局配置)

## 背景

当前架构中，设备级指标（核心利用率、显存、温度、功率）遵循"设备类型指标配方"模式
（`DeviceSpec`），每种设备类型独立声明自己的指标名和标签映射。但主机指标（CPU、内存、
句柄数）位于独立的全局 `HostMetricsConfig` 中，与设备类型完全解耦。

这造成两个问题：
1. **设计不一致**：设备指标用配方，主机指标用全局配置，概念割裂
2. **灵活性不足**：不同设备类型可能需要不同的主机指标来源或标签名

## 设计

### 1. 新增 `HostMetricsSpec` 结构体

在 `devices.rs` 中新增：

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct HostMetricsSpec {
    pub cpu_metric: String,
    pub mem_metric: String,
    #[serde(default)]
    pub handle_metric: Option<String>,
    #[serde(default = "default_host_label")]
    pub host_label: String,
}
```

### 2. 扩展 `DeviceSpec`

在 `DeviceSpec` 中新增可选字段：

```rust
pub struct DeviceSpec {
    // ... 现有字段 ...
    #[serde(default)]
    pub host_metrics: Option<HostMetricsSpec>,
}
```

### 3. 移除全局 `HostMetricsConfig`

- 从 `config.rs` 删除 `HostMetricsConfig` 结构体
- 从 `AppConfig` 删除 `host_metrics: Option<HostMetricsConfig>` 字段
- 添加向后兼容的 `LegacyHostMetricsConfig` 字段（与 `HostIpConfig` 同模式），
  旧配置文件中的 `host_metrics:` 键不会导致 `deny_unknown_fields` 报错，
  但内容被忽略，日志输出迁移提示
- `default_config_yaml()` 中主机指标配置从"八、主机指标采集"独立节移入
  "四、设备类型指标配方"的各设备块内

### 4. 主机指标采集逻辑移入 `pipeline::collect_device`

当前 `main.rs` 中有一个独立的主机指标采集循环（步骤 5.5），遍历所有唯一主机 IP，
从全局配置的 `host_metrics` 取指标名，构造 PromQL 查询，结果填入 `CardRecord`。

变更后：
- 主机指标采集在 `collect_device` 内完成，与设备指标同流程
- **采集时机**：在 `groups` HashMap 构建完成后、`for key in keys` 记录构建循环之前，
  从 `groups.keys()` 提取唯一主机 IP，查询主机指标，存入
  `HashMap<String, HostMetricValues>`，在记录构建时填入
- 主机 IP 匹配标签从 `spec.labels.host_ip` 获取（复用已有的 IP 提取逻辑）
- **同源查询**：主机指标使用与设备指标相同的 Prometheus 数据源（同一 fetcher），
  不再需要 `source` 字段。这是合理的：设备配方及其主机指标都从定义了
  `device_types` 的同一源查询
- `CardRecord` 的主机指标字段不变
- **注意**：同一主机若同时有 NVIDIA 和 NPU 卡，主机指标会被查询两次
  （每个设备类型各一次）。若两者配置相同，结果一致；若配置不同，
  各设备类型的卡行将显示各自配置对应的主机指标值

### 5. `mapper.rs` 列标志位计算

`compute_column_flags` 改为检查设备配方中的 `host_metrics` 字段，而非全局配置：

```rust
if spec.host_metrics.is_some() {
    flags.has_host_cpu = true;
    flags.has_host_mem = true;
    flags.has_host_handle = spec.host_metrics.as_ref()
        .and_then(|h| h.handle_metric.as_ref()).is_some();
}
```

### 6. 预设配方更新

`nvidia_a10_spec()` 和 `ascend_910b_spec()` 均添加注释示例的 `host_metrics` 配置
（默认为 `None`，用户按需启用）。

### 7. 配置校验更新

- 删除全局 `host_metrics` 的校验逻辑
- 在设备配方校验循环中新增 `spec.host_metrics` 的指标名/标签名合法性校验
- 校验规则与原全局校验一致：`cpu_metric`/`mem_metric` 必须是合法 Prometheus 指标名，
  `handle_metric`（可选）同理，`host_label` 必须是合法标签名

## YAML 配置形态

变更前（全局配置）：

```yaml
host_metrics:
  enabled: true
  source: "prod-cluster"
  cpu_metric: "node_cpu_utilization"
  mem_metric: "node_memory_utilization"
  handle_metric: "node_filefd_allocated"
  host_label: "instance"
```

变更后（设备配方内嵌）：

```yaml
devices:
  nvidia_a10:
    display_name: "NVIDIA A10"
    core_util_metric: "DCGM_FI_DEV_GPU_UTIL"
    # ...
    host_metrics:
      cpu_metric: "node_cpu_utilization"
      mem_metric: "node_memory_utilization"
      handle_metric: "node_filefd_allocated"
      host_label: "instance"
  ascend_910b:
    display_name: "Ascend 910B"
    core_util_metric: "npu_chip_info_utilization"
    # ...
    host_metrics:
      cpu_metric: "node_cpu_utilization"
      mem_metric: "node_memory_utilization"
      host_label: "instance"
```

## 变更文件清单

| 文件 | 变更类型 | 说明 |
|------|----------|------|
| `src/devices.rs` | 修改 | 新增 `HostMetricsSpec`，`DeviceSpec` 加字段，预设更新 |
| `src/config.rs` | 修改 | 删除 `HostMetricsConfig`，加兼容字段，校验更新，默认配置更新 |
| `src/main.rs` | 修改 | 删除独立主机指标采集循环，调整 `compute_column_flags` 调用 |
| `src/pipeline.rs` | 修改 | `collect_device` 内新增主机指标采集逻辑 |
| `src/mapper.rs` | 修改 | `compute_column_flags` 签名和逻辑更新 |
| `Cargo.toml` | 修改 | 版本号 → 1.8.0 |

## 测试要点

1. `DeviceSpec` 含 `host_metrics` 时 YAML 往返序列化正确
2. `DeviceSpec` 无 `host_metrics` 时 `Option` 默认为 `None`
3. 配置校验：非法指标名/标签名在 `host_metrics` 中被拒绝
4. `collect_device` 在 `host_metrics` 配置下正确采集主机指标
5. 多设备类型各配不同 `host_metrics` 时互不干扰
6. 旧配置文件含全局 `host_metrics` 时不报错（兼容忽略）
7. `compute_column_flags` 正确检测设备配方中的 `host_metrics`
8. 默认配置模板可解析且主机指标在设备块内
