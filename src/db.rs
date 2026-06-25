//! MySQL 数据库推送模块。
//!
//! 职责：根据配置将采集结果写入 MySQL 表，包含：
//! - 自动建表（含列注释）
//! - schema 校验（缺列→生成DDL退出；多余列→询问用户）
//! - 逐行 INSERT
//!
//! 列映射通过本地字段名（local_name）而非报表显示名进行，
//! 确保映射不受显示名变化影响。用户可在配置中指定 db_type 覆盖自动推断。

use crate::config::DatabaseConfig;
use crate::error::AppError;
use crate::processor::CardRecord;
use chrono_tz::Tz;
use sqlx::{MySql, Pool};
use std::collections::{HashMap, HashSet};
use tracing::{error, info, warn};

/// 数据库推送的入口。
///
/// 1. 连接 MySQL
/// 2. 检查/创建表结构
/// 3. 逐行 INSERT
///
/// # Errors
///
/// 返回 [`AppError::Database`] 当连接失败、schema 不匹配或写入失败。
pub async fn push_to_database(
    records: &[CardRecord],
    cfg: &DatabaseConfig,
    mapping_values: &HashMap<(usize, String), String>,
    base_columns: &[String],
    base_local_names: &[String],
    mapping_columns: &[crate::mapper::MappingColumn],
    tz: Tz,
) -> Result<(), AppError> {
    if records.is_empty() {
        info!("无记录，跳过数据库推送");
        return Ok(());
    }

    // 基础列显示名和本地字段名必须一一对应
    debug_assert_eq!(
        base_columns.len(),
        base_local_names.len(),
        "base_columns 和 base_local_names 长度不一致"
    );

    // 构建 display_name → local_name 的映射（基础列 + 映射列）
    // 先收集映射列的 effective_local_name 到 owned Vec，避免 lifetime 问题
    let mapping_local_names_owned: Vec<String> = mapping_columns
        .iter()
        .map(|c| c.effective_local_name().to_string())
        .collect();
    let display_to_local: HashMap<&str, &str> = base_columns
        .iter()
        .zip(base_local_names.iter())
        .map(|(d, l)| (d.as_str(), l.as_str()))
        .chain(
            mapping_columns
                .iter()
                .zip(mapping_local_names_owned.iter())
                .map(|(c, ln)| (c.rename.as_str(), ln.as_str())),
        )
        .collect();

    // 构建 local_name → db_name + db_type + comment 的索引
    let col_map: HashMap<&str, (&str, Option<&str>, &str)> = cfg
        .columns
        .iter()
        .map(|c| {
            (
                c.local_name.as_str(),
                (c.db_name.as_str(), c.db_type.as_deref(), c.comment.as_str()),
            )
        })
        .collect();

    // 过滤出有映射的列：按 order 中的显示列名 → local_name → db 映射
    // mapped_cols: (display_name, local_name, db_name, db_type, comment)
    let base_refs: Vec<&str> = base_columns.iter().map(String::as_str).collect();
    let order = crate::mapper::compute_column_order(&base_refs, mapping_columns);
    let mapped_cols: Vec<(&str, &str, &str, Option<&str>, &str)> = order
        .iter()
        .filter_map(|display_name| {
            let local_name = display_to_local.get(display_name.as_str())?;
            col_map.get(local_name).map(|(db, db_type, cmt)| {
                (display_name.as_str(), *local_name, *db, *db_type, *cmt)
            })
        })
        .collect();

    // 警告：配置了但不在当前活跃列中的映射（可能属于未启用的指标组）
    let configured_local_names: HashSet<&str> =
        cfg.columns.iter().map(|c| c.local_name.as_str()).collect();
    let active_local_names: HashSet<&str> =
        display_to_local.values().copied().collect();
    for name in &configured_local_names - &active_local_names {
        warn!(
            "database.columns 中 local_name「{name}」不在当前活跃报表列中（可能属于未启用的指标组），已跳过"
        );
    }

    if mapped_cols.is_empty() {
        return Err(AppError::Database {
            detail: "database.columns 中没有匹配任何本地字段名的映射".into(),
        });
    }

    // 连接数据库（30 秒连接超时，防止不可达时无限挂起）
    let url = build_mysql_url(cfg);
    info!("连接 MySQL：{}:{}", cfg.host, cfg.port);
    let pool = sqlx::pool::PoolOptions::<MySql>::new()
        .acquire_timeout(std::time::Duration::from_secs(30))
        .connect(&url)
        .await
        .map_err(|e| AppError::Database {
            detail: format!(
                "无法连接 MySQL {}:{} 数据库「{}」：{e}",
                cfg.host, cfg.port, cfg.database
            ),
        })?;

    let result = async {
        // 检查/创建表
        ensure_table(&pool, cfg, &mapped_cols).await?;

        // 逐行 INSERT
        let count = insert_records(&pool, cfg, records, &mapped_cols, mapping_values, tz)
            .await?;

        info!("数据库推送完成：{count} 行写入 {db}.{table}", db = cfg.database, table = cfg.table);
        Ok::<(), AppError>(())
    }
    .await;

    // 无论成功还是失败，都优雅关闭连接池
    pool.close().await;
    result
}

/// 构建 MySQL 连接 URL。
fn build_mysql_url(cfg: &DatabaseConfig) -> String {
    // IPv6 地址需用方括号包裹（RFC 3986）
    let host_part = if cfg.host.contains(':') && !cfg.host.starts_with('[') {
        format!("[{}]", cfg.host)
    } else {
        cfg.host.clone()
    };
    format!(
        "mysql://{}:{}@{}:{}/{}",
        percent_encode(&cfg.username),
        percent_encode(&cfg.password),
        host_part,
        cfg.port,
        percent_encode(&cfg.database)
    )
}

/// 对 MySQL URL 中的用户名/密码做百分号编码（防特殊字符导致连接失败）。
fn percent_encode(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
                vec![c]
            } else {
                let mut buf = [0u8; 4];
                let bytes = c.encode_utf8(&mut buf).as_bytes();
                bytes.iter().flat_map(|b| vec!['%', hex(b >> 4), hex(b & 0xF)]).collect()
            }
        })
        .collect()
}

fn hex(n: u8) -> char {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    HEX[n as usize] as char
}

/// 检测 stdin 是否连接到终端（交互模式）。
/// 非交互环境（CI/CD、cron、管道输入）返回 false。
fn atty_is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// 检查表是否存在并验证 schema；不存在则创建。
///
/// - 表不存在 → 自动 CREATE TABLE
/// - 表存在但缺列 → 生成 DDL 文件并退出
/// - 表存在但有多余列 → 询问用户
async fn ensure_table(
    pool: &Pool<MySql>,
    cfg: &DatabaseConfig,
    mapped_cols: &[(&str, &str, &str, Option<&str>, &str)],
) -> Result<(), AppError> {
    let table_exists = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = ? AND table_name = ?",
    )
    .bind(&cfg.database)
    .bind(&cfg.table)
    .fetch_one(pool)
    .await
    .map_err(|e| AppError::Database {
        detail: format!("查询 information_schema 失败：{e}"),
    })?
        > 0;

    if !table_exists {
        info!("表 {}.{} 不存在，自动创建", cfg.database, cfg.table);
        let ddl = generate_create_ddl(cfg, mapped_cols);
        sqlx::query(&ddl)
            .execute(pool)
            .await
            .map_err(|e| AppError::Database {
                detail: format!("创建表 {table} 失败：{e}", table = cfg.table),
            })?;
        info!("表 {}.{} 创建成功", cfg.database, cfg.table);
        return Ok(());
    }

    // 表存在：获取现有列
    let existing_columns = get_table_columns(pool, cfg).await?;
    let configured_db_names: HashSet<&str> =
        mapped_cols.iter().map(|(_, _, db, _, _)| *db).collect();
    let existing_db_names: HashSet<&str> = existing_columns.keys().map(String::as_str).collect();

    // 检查缺少的列
    let missing: Vec<&str> = configured_db_names
        .difference(&existing_db_names)
        .copied()
        .collect();

    if !missing.is_empty() {
        let ddl = generate_alter_ddl(cfg, mapped_cols, &missing);
        let ddl_path = format!("{}_alter.sql", cfg.table);
        std::fs::write(&ddl_path, &ddl).map_err(|e| AppError::Database {
            detail: format!("写入 DDL 文件失败：{e}"),
        })?;
        error!(
            "表 {}.{} 缺少 {} 列：{}，DDL 已写入 {ddl_path}，请执行后重新运行",
            cfg.database,
            cfg.table,
            missing.len(),
            missing.join(", ")
        );
        return Err(AppError::Database {
            detail: format!(
                "表缺少列，DDL 已写入 {ddl_path}：{}",
                missing.join(", ")
            ),
        });
    }

    // 检查多余的列
    let extra: Vec<&str> = existing_db_names
        .difference(&configured_db_names)
        .copied()
        .collect();

    if !extra.is_empty() {
        warn!(
            "表 {}.{} 有 {} 列未在配置中映射：{}",
            cfg.database,
            cfg.table,
            extra.len(),
            extra.join(", ")
        );
        // 交互式询问用户（非交互环境默认忽略多余列并继续）
        let is_interactive = atty_is_terminal();
        if is_interactive {
            println!(
                "\n[提示] 表 {}.{} 中存在未配置的列：{}\n  [I] 忽略多余列，继续写入\n  [Q] 退出并生成 DDL SQL 文件\n请选择 (I/Q)：",
                cfg.database, cfg.table, extra.join(", ")
            );
            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .map_err(|e| AppError::Database {
                    detail: format!("读取用户输入失败：{e}"),
                })?;
            match input.trim().to_uppercase().as_str() {
                "I" => {
                    info!("用户选择忽略多余列，继续写入");
                }
                _ => {
                    let ddl_path = format!("{}_extra_columns.sql", cfg.table);
                    let content = format!(
                        "-- 表 {}.{} 中存在但配置未映射的列\n-- 列名：{}\n-- 如需删除请手动执行：\n\n{}",
                        cfg.database,
                        cfg.table,
                        extra.join(", "),
                        extra
                            .iter()
                            .map(|col| format!(
                                "ALTER TABLE `{}` DROP COLUMN `{}`;",
                                cfg.table, col
                            ))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                    std::fs::write(&ddl_path, &content).map_err(|e| AppError::Database {
                        detail: format!("写入 DDL 文件失败：{e}"),
                    })?;
                    info!("DDL 已写入 {ddl_path}，请处理后重新运行");
                    return Err(AppError::Database {
                        detail: format!("用户选择退出，DDL 已写入 {ddl_path}"),
                    });
                }
            }
        } else {
            // 非交互环境（CI/CD、cron）：默认忽略多余列，继续写入
            info!("非交互环境，自动忽略多余列，继续写入");
        }
    }

    Ok(())
}

/// 获取表的现有列名和类型。
async fn get_table_columns(
    pool: &Pool<MySql>,
    cfg: &DatabaseConfig,
) -> Result<HashMap<String, String>, AppError> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT COLUMN_NAME, DATA_TYPE FROM information_schema.columns WHERE table_schema = ? AND table_name = ? ORDER BY ORDINAL_POSITION")
            .bind(&cfg.database)
            .bind(&cfg.table)
            .fetch_all(pool)
            .await
            .map_err(|e| AppError::Database {
                detail: format!("查询表结构失败：{e}"),
            })?;
    Ok(rows.into_iter().collect())
}

/// 转义 MySQL 字符串字面量中的特殊字符（用于 DDL COMMENT）。
fn escape_mysql_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "''")
        .replace('\0', "\\0")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// 生成 CREATE TABLE DDL。
fn generate_create_ddl(cfg: &DatabaseConfig, mapped_cols: &[(&str, &str, &str, Option<&str>, &str)]) -> String {
    let mut lines = Vec::new();
    for (_display_name, local_name, db_name, db_type, comment) in mapped_cols {
        let sql_type = db_type.unwrap_or_else(|| infer_sql_type(local_name));
        let comment_escaped = escape_mysql_string(comment);
        lines.push(format!(
            "  `{db_name}` {sql_type} COMMENT '{comment_escaped}'"
        ));
    }
    format!(
        "CREATE TABLE `{table}` (\n{cols}\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci;",
        table = cfg.table,
        cols = lines.join(",\n")
    )
}

/// 生成 ALTER TABLE DDL（添加缺少的列）。
fn generate_alter_ddl(
    cfg: &DatabaseConfig,
    mapped_cols: &[(&str, &str, &str, Option<&str>, &str)],
    missing: &[&str],
) -> String {
    let mut lines = Vec::new();
    for (_display_name, local_name, db_name, db_type, comment) in mapped_cols {
        if missing.contains(db_name) {
            let sql_type = db_type.unwrap_or_else(|| infer_sql_type(local_name));
            let comment_escaped = escape_mysql_string(comment);
            lines.push(format!(
                "ADD COLUMN `{db_name}` {sql_type} COMMENT '{comment_escaped}'"
            ));
        }
    }
    format!(
        "-- 为表 {table} 添加缺少的列\nALTER TABLE `{table}`\n{adds};",
        table = cfg.table,
        adds = lines.join(",\n")
    )
}

/// 根据本地字段名推断 MySQL 数据类型。
///
/// 字段名是稳定标识符（如 "time_range"、"core_avg"），不受显示名变化影响。
fn infer_sql_type(local_name: &str) -> &'static str {
    // 时间范围 → VARCHAR（"time_range" 是文本，不是单个时间点）
    if local_name == "time_range" || local_name.ends_with("_range") {
        return "VARCHAR(64) DEFAULT NULL";
    }
    // 时间列 → DATETIME
    if local_name.ends_with("_time") {
        return "DATETIME DEFAULT NULL";
    }
    // 数据量 → INT
    if local_name.ends_with("_count") {
        return "INT DEFAULT NULL";
    }
    // 句柄数 → DOUBLE（可能是非整数）
    if local_name.starts_with("host_handle_") && !local_name.ends_with("_time") && !local_name.ends_with("_count") {
        return "DOUBLE DEFAULT NULL";
    }
    // 百分比/绝对值列 → DOUBLE（avg/peak 后缀）
    if local_name.ends_with("_avg") || local_name.ends_with("_peak") {
        return "DOUBLE DEFAULT NULL";
    }
    // 文本列 → VARCHAR
    "VARCHAR(255) DEFAULT NULL"
}

/// 逐行 INSERT 记录到数据库。
async fn insert_records(
    pool: &Pool<MySql>,
    cfg: &DatabaseConfig,
    records: &[CardRecord],
    mapped_cols: &[(&str, &str, &str, Option<&str>, &str)],
    mapping_values: &HashMap<(usize, String), String>,
    tz: Tz,
) -> Result<usize, AppError> {
    // 构建 mapping_borrowed 索引
    let mapping_borrowed: HashMap<(usize, &str), &str> = mapping_values
        .iter()
        .map(|((row, col), val)| ((*row, col.as_str()), val.as_str()))
        .collect();

    // 构建列名和占位符
    let db_names: Vec<&str> = mapped_cols.iter().map(|(_, _, db, _, _)| *db).collect();
    let placeholders: Vec<&str> = mapped_cols.iter().map(|_| "?").collect();

    let sql = format!(
        "INSERT INTO `{table}` ({cols}) VALUES ({vals})",
        table = cfg.table,
        cols = db_names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", "),
        vals = placeholders.join(", ")
    );

    // 开启事务，确保所有 INSERT 原子提交或整体回滚，避免部分写入留下不完整数据
    let mut tx = pool.begin().await.map_err(|e| AppError::Database {
        detail: format!("开启事务失败：{e}"),
    })?;

    let mut count = 0usize;
    let mut first_error: Option<String> = None;
    for (row_idx, rec) in records.iter().enumerate() {
        // 构建该行的值（按 mapped_cols 顺序，使用 display_name 取值）
        let values: Vec<Option<String>> = mapped_cols
            .iter()
            .map(|(display_name, _local_name, _db_name, _db_type, _comment)| {
                crate::reporter::cell_value_for_db(rec, display_name, &mapping_borrowed, row_idx, tz)
            })
            .collect();

        // 执行 INSERT
        let mut query = sqlx::query(&sql);
        for val in &values {
            match val {
                Some(v) => query = query.bind(v),
                None => query = query.bind(None::<String>),
            }
        }
        match query.execute(&mut *tx).await {
            Ok(_) => count += 1,
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(format!("{e}"));
                }
                warn!("写入第 {} 行失败：{e}", row_idx + 1);
                // 首次失败后立即跳出循环，避免浪费数据库往返
                break;
            }
        }
    }

    if count == 0 {
        // 全部失败：回滚事务
        let _ = tx.rollback().await;
        return Err(AppError::Database {
            detail: format!(
                "所有 {total} 行写入均失败，首条错误：{e}",
                total = records.len(),
                e = first_error.expect("at least one error when count==0")
            ),
        });
    }
    if count < records.len() {
        warn!(
            "部分行写入失败：{count}/{} 行成功，回滚事务以保证数据一致性",
            records.len()
        );
        let _ = tx.rollback().await;
        return Err(AppError::Database {
            detail: format!(
                "部分行写入失败（{count}/{} 成功），已回滚事务，首条错误：{e}",
                records.len(),
                e = first_error.expect("first_error set when count < records.len()")
            ),
        });
    }

    // 全部成功：提交事务
    tx.commit().await.map_err(|e| AppError::Database {
        detail: format!("提交事务失败：{e}"),
    })?;

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encode_handles_special_chars() {
        assert_eq!(percent_encode("hello"), "hello");
        assert_eq!(percent_encode("user@host"), "user%40host");
        assert_eq!(percent_encode("p@ss:word"), "p%40ss%3Aword");
        assert_eq!(percent_encode("simple123"), "simple123");
    }

    #[test]
    fn infer_sql_type_pct_columns() {
        assert_eq!(infer_sql_type("core_avg"), "DOUBLE DEFAULT NULL");
        assert_eq!(infer_sql_type("mem_peak"), "DOUBLE DEFAULT NULL");
        assert_eq!(infer_sql_type("host_cpu_avg"), "DOUBLE DEFAULT NULL");
    }

    #[test]
    fn infer_sql_type_number_columns() {
        assert_eq!(infer_sql_type("temp_avg"), "DOUBLE DEFAULT NULL");
        assert_eq!(infer_sql_type("power_peak"), "DOUBLE DEFAULT NULL");
    }

    #[test]
    fn infer_sql_type_count_columns() {
        assert_eq!(infer_sql_type("core_count"), "INT DEFAULT NULL");
        assert_eq!(infer_sql_type("mem_count"), "INT DEFAULT NULL");
    }

    #[test]
    fn infer_sql_type_time_columns() {
        assert_eq!(infer_sql_type("core_peak_time"), "DATETIME DEFAULT NULL");
        assert_eq!(infer_sql_type("host_cpu_peak_time"), "DATETIME DEFAULT NULL");
        assert_eq!(infer_sql_type("host_handle_peak_time"), "DATETIME DEFAULT NULL");
    }

    #[test]
    fn infer_sql_type_handle_columns() {
        assert_eq!(infer_sql_type("host_handle_avg"), "DOUBLE DEFAULT NULL");
        assert_eq!(infer_sql_type("host_handle_peak"), "DOUBLE DEFAULT NULL");
    }

    #[test]
    fn infer_sql_type_text_columns() {
        assert_eq!(infer_sql_type("source_name"), "VARCHAR(255) DEFAULT NULL");
        assert_eq!(infer_sql_type("host_ip"), "VARCHAR(255) DEFAULT NULL");
        assert_eq!(infer_sql_type("time_range"), "VARCHAR(64) DEFAULT NULL");
    }

    #[test]
    fn escape_mysql_string_handles_special_chars() {
        assert_eq!(escape_mysql_string("hello"), "hello");
        assert_eq!(escape_mysql_string("it's"), "it''s");
        assert_eq!(escape_mysql_string("path\\file"), "path\\\\file");
        assert_eq!(escape_mysql_string("line1\nline2"), "line1\\nline2");
        assert_eq!(escape_mysql_string("cr\r\nlf"), "cr\\r\\nlf");
        assert_eq!(escape_mysql_string("null\0byte"), "null\\0byte");
    }

    #[test]
    fn generate_create_ddl_format() {
        let cfg = DatabaseConfig {
            enabled: true,
            host: "localhost".into(),
            port: 3306,
            username: "root".into(),
            password: String::new(),
            database: "test".into(),
            table: "gpu_util".into(),
            columns: vec![],
        };
        let cols = vec![
            ("主机IP", "host_ip", "host_ip", None, "主机IP地址"),
            ("核心利用率平均值", "core_avg", "core_avg", None, "核心利用率平均值"),
        ];
        let ddl = generate_create_ddl(&cfg, &cols);
        assert!(ddl.starts_with("CREATE TABLE `gpu_util`"));
        assert!(ddl.contains("`host_ip` VARCHAR(255)"));
        assert!(ddl.contains("`core_avg` DOUBLE"));
        assert!(ddl.contains("COMMENT '主机IP地址'"));
        assert!(ddl.contains("ENGINE=InnoDB"));
    }

    #[test]
    fn generate_create_ddl_with_custom_type() {
        let cfg = DatabaseConfig {
            enabled: true,
            host: "localhost".into(),
            port: 3306,
            username: "root".into(),
            password: String::new(),
            database: "test".into(),
            table: "gpu_util".into(),
            columns: vec![],
        };
        let cols = vec![
            ("取值时间范围", "time_range", "time_range", Some("VARCHAR(128) DEFAULT NULL"), "数据采集时间范围"),
        ];
        let ddl = generate_create_ddl(&cfg, &cols);
        assert!(ddl.contains("`time_range` VARCHAR(128) DEFAULT NULL"));
    }
}
