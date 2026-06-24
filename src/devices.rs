//! 设备类型"指标配方"模块。
//!
//! 把 PRD §2.2 中 NVIDIA A10 与 Ascend 910B 的指标抽取规则抽象成数据
//! （[`DeviceSpec`]），让 fetcher/processor 据此决定查什么、怎么算，
//! 而不是把设备特定逻辑写死在代码分支里。新增设备类型只需新增一份配方。

use serde::{Deserialize, Serialize};

/// 标签名映射：把统一的逻辑归属字段映射到各 exporter 实际使用的标签名。
///
/// 例如 NPU 用 `container_name`/`pod_name`，DCGM 常用 `container`/`pod`。
/// `host_ip` 和 `node_name` 也在此配置，因为不同 exporter 使用的标签名不同
/// （如 DCGM 常用 `ip`，NPU 可能用 `host_ip`；节点名可能是 `node` 或 `nodename`）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LabelMapping {
    /// 主机 IP 标签名（优先取此标签，取不到时从 instance 标签去端口解析）。
    pub host_ip: String,
    /// 节点名称标签名。
    pub node_name: String,
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
/// `composite_from_total: { used, total }`（外部命名的单键 map），兼容 `serde_yaml_ng`
/// 对带字段变体的限制（`serde_yaml_ng` 不支持默认的 externally-tagged 字段变体）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum MemoryStrategy {
    /// used/(used+free)*100 组合（如 GPU 的 `FB_USED/(FB_USED+FB_FREE)`）。
    CompositeRatio(CompositeRatioBody),
    /// 直接读一个利用率指标（如 NPU 的 `npu_chip_info_hbm_utilization`）。
    /// `fallback` 在该指标查询为空时启用。
    DirectMetric(DirectMetricBody),
    /// used/total*100 组合（如 NPU fallback 的 `hbm_used/hbm_total`）。
    CompositeFromTotal(CompositeFromTotalBody),
}

/// `composite_ratio` 包装体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompositeRatioBody {
    pub composite_ratio: UsedFree,
}
/// `composite_from_total` 包装体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompositeFromTotalBody {
    pub composite_from_total: UsedTotal,
}
/// `direct_metric` 包装体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectMetricBody {
    pub direct_metric: DirectInner,
}
/// used/free 两个字段。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UsedFree {
    pub used: String,
    pub free: String,
}
/// used/total 两个字段。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UsedTotal {
    pub used: String,
    pub total: String,
}
/// direct 指标 + 可选 fallback。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DirectInner {
    pub metric: String,
    #[serde(default)]
    pub fallback: Option<Box<MemoryStrategy>>,
}

impl MemoryStrategy {
    /// 便捷构造：GPU 组合公式。
    #[must_use]
    pub fn composite_ratio(used: &str, free: &str) -> Self {
        Self::CompositeRatio(CompositeRatioBody {
            composite_ratio: UsedFree {
                used: used.into(),
                free: free.into(),
            },
        })
    }
    /// 便捷构造：NPU used/total 组合。
    #[must_use]
    pub fn composite_from_total(used: &str, total: &str) -> Self {
        Self::CompositeFromTotal(CompositeFromTotalBody {
            composite_from_total: UsedTotal {
                used: used.into(),
                total: total.into(),
            },
        })
    }
    /// 便捷构造：direct 指标 + 可选 fallback。
    pub fn direct(metric: &str, fallback: Option<Self>) -> Self {
        Self::DirectMetric(DirectMetricBody {
            direct_metric: DirectInner {
                metric: metric.into(),
                fallback: fallback.map(Box::new),
            },
        })
    }
}

impl MemoryStrategy {
    /// 返回首个可用的指标名，供归属查询回退使用（当核心指标无数据时，
    /// 用显存指标查询归属标签）。
    ///
    /// - `CompositeRatio`: 返回 `used` 指标名
    /// - `DirectMetric`: 返回 `metric` 指标名
    /// - `CompositeFromTotal`: 返回 `used` 指标名
    #[must_use]
    pub fn first_metric_name(&self) -> Option<&str> {
        match self {
            Self::CompositeRatio(b) => Some(&b.composite_ratio.used),
            Self::DirectMetric(b) => Some(&b.direct_metric.metric),
            Self::CompositeFromTotal(b) => Some(&b.composite_from_total.used),
        }
    }

    /// 返回所有可能含归属标签的指标名列表（含 fallback 链），供归属查询依次尝试。
    ///
    /// 当 `DirectMetric` 的主指标无数据时，其 fallback 的 `used` 指标仍可能有数据
    /// 和归属标签（namespace/pod/container），因此需要逐级尝试。
    #[must_use]
    pub fn ownership_metric_names(&self) -> Vec<&str> {
        match self {
            Self::CompositeRatio(b) => vec![&b.composite_ratio.used],
            Self::DirectMetric(b) => {
                let mut names = vec![&b.direct_metric.metric as &str];
                if let Some(fb) = &b.direct_metric.fallback {
                    names.extend(fb.ownership_metric_names());
                }
                names
            }
            Self::CompositeFromTotal(b) => vec![&b.composite_from_total.used],
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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
#[must_use]
pub fn nvidia_a10_spec() -> DeviceSpec {
    DeviceSpec {
        display_name: "NVIDIA A10".into(),
        core_util_metric: "DCGM_FI_DEV_GPU_UTIL".into(),
        memory: MemoryStrategy::composite_ratio("DCGM_FI_DEV_FB_USED", "DCGM_FI_DEV_FB_FREE"),
        card_id_label: "gpu".into(),
        labels: LabelMapping {
            host_ip: "host_ip".into(),
            node_name: "pod_node".into(),
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
#[must_use]
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
            host_ip: "host_ip".into(),
            node_name: "endpoint_node".into(),
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
            other => panic!("期望 CompositeRatio，得到 {other:?}"),
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
            other => panic!("期望 DirectMetric，得到 {other:?}"),
        }
    }

    #[test]
    fn device_spec_round_trips_through_yaml() {
        let s = ascend_910b_spec();
        let yaml = serde_yaml_ng::to_string(&s).unwrap();
        let back: DeviceSpec = serde_yaml_ng::from_str(&yaml).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn memory_strategy_untagged_rejects_ambiguous_yaml() {
        // serde(untagged) + deny_unknown_fields：当 YAML 同时含 composite_ratio 和
        // direct_metric 键时，两个变体都因多余字段被拒绝，返回解析错误。
        // 这是正确行为——含糊配置不应静默匹配到错误变体。
        let yaml = r#"
composite_ratio:
  used: "metric_a"
  free: "metric_b"
direct_metric:
  metric: "metric_c"
"#;
        let result: Result<MemoryStrategy, _> = serde_yaml_ng::from_str(yaml);
        assert!(
            result.is_err(),
            "含糊的 YAML（同时含多个变体键）应被拒绝，而非静默匹配"
        );
    }
}
