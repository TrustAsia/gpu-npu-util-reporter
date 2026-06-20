# CLAUDE.md

## 版本号规则

每次代码修改**必须**同步更新 `Cargo.toml` 中的 `version` 字段，遵循语义化版本（semver）：

- **PATCH** `x.y.z+1`：bug 修复、CI 调整、文档更新等不改变功能的改动
- **MINOR** `x.y+1.0`：新增功能，向后兼容
- **MAJOR** `x+1.0.0`：破坏性变更

CI Release 行为：
- 版本号 tag 不存在时 → 创建 `v{version}` Release（正式版）
- 版本号 tag 已存在时 → 创建 `v{version}+{short_sha}` Release（自动标记为 prerelease）
