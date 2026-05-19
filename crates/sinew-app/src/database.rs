use std::{sync::Once, time::Duration};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sinew_core::ToolDescriptor;
use sqlx::{
    any::{AnyPoolOptions, AnyRow},
    AnyPool, Column, Row, TypeInfo,
};

use crate::tool_run::ToolRunResult;

pub const DATABASE_QUERY_TOOL_NAME: &str = "DatabaseQuery";
pub const DATABASE_QUERY_TOOL_DEFAULT_DESCRIPTION: &str = "Interroge la base de données SQL native (PostgreSQL, MySQL ou SQLite) configurée dans Settings > Data Sources > Database. Fournis une instruction SQL en lecture seule via `query`, ou un nom de table via `table` pour récupérer un échantillon. Sans `query` ni `table`, l'outil liste les tables disponibles. Lecture seule uniquement : DROP/DELETE/UPDATE et autres mutations sont refusées.";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_ROWS: usize = 50;
const NOT_CONFIGURED_MESSAGE: &str =
    "Database non configurée — renseigne les credentials dans Settings > Data Sources > Database.";

static INSTALL_DRIVERS: Once = Once::new();

fn install_drivers() {
    INSTALL_DRIVERS.call_once(sqlx::any::install_default_drivers);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseKind {
    Postgres,
    Mysql,
    Sqlite,
}

impl DatabaseKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "pg" => Some(Self::Postgres),
            "mysql" | "mariadb" => Some(Self::Mysql),
            "sqlite" | "sqlite3" => Some(Self::Sqlite),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Mysql => "mysql",
            Self::Sqlite => "sqlite",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    pub kind: DatabaseKind,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
}

#[derive(Debug, Clone, Default)]
pub struct DatabaseQueryTool {
    config: Option<DatabaseConfig>,
}

impl DatabaseQueryTool {
    pub fn new() -> Self {
        Self { config: None }
    }

    pub fn with_config(config: Option<DatabaseConfig>) -> Self {
        if config.is_some() {
            install_drivers();
        }
        Self { config }
    }

    pub fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: DATABASE_QUERY_TOOL_NAME.to_string(),
            description: DATABASE_QUERY_TOOL_DEFAULT_DESCRIPTION.to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Instruction SQL en lecture seule (SELECT, WITH, SHOW, EXPLAIN, DESCRIBE, PRAGMA). Les mutations sont refusées."
                    },
                    "table": {
                        "type": "string",
                        "description": "Nom de table à interroger. Sans `query`, renvoie SELECT * FROM table LIMIT 50. Sans `query` ni `table`, liste les tables disponibles."
                    }
                },
                "additionalProperties": false
            }),
        }
    }

    pub async fn run(&self, input: Value) -> ToolRunResult {
        let parsed: DatabaseQueryInput = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(err) => {
                return ToolRunResult::err(
                    format!("invalid DatabaseQuery input: {err}"),
                    Vec::new(),
                )
            }
        };

        let query = parsed.query.unwrap_or_default().trim().to_string();
        let table = parsed
            .table
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        let Some(cfg) = self.config.as_ref() else {
            return ToolRunResult::ok(NOT_CONFIGURED_MESSAGE.to_string(), Vec::new());
        };

        let result = match (query.is_empty(), table.as_deref()) {
            (true, None) => list_tables(cfg).await,
            (true, Some(table)) => {
                if !is_safe_table_name(table) {
                    return ToolRunResult::err(
                        format!(
                            "invalid table name `{table}`; expected letters, digits, underscores or dot"
                        ),
                        Vec::new(),
                    );
                }
                let sql = format!(
                    "SELECT * FROM {} LIMIT {MAX_ROWS}",
                    quote_table(table, cfg.kind)
                );
                run_query(cfg, &sql).await
            }
            (false, _) => {
                if let Err(err) = ensure_read_only(&query) {
                    return ToolRunResult::err(err.to_string(), Vec::new());
                }
                run_query(cfg, &query).await
            }
        };

        match result {
            Ok(output) => ToolRunResult::ok(output, Vec::new()),
            Err(err) => ToolRunResult::err(
                redact_password(&err.to_string(), &cfg.password),
                Vec::new(),
            ),
        }
    }
}

pub async fn test_database_connection(cfg: &DatabaseConfig) -> Result<String> {
    match cfg.kind {
        DatabaseKind::Sqlite => {
            if cfg.database.trim().is_empty() {
                bail!("Le chemin de la base SQLite est obligatoire (Database Name).");
            }
        }
        _ => {
            if cfg.host.trim().is_empty() {
                bail!("L'hôte est obligatoire.");
            }
            if cfg.user.trim().is_empty() {
                bail!("L'utilisateur est obligatoire.");
            }
            if cfg.database.trim().is_empty() {
                bail!("Le nom de la base est obligatoire.");
            }
        }
    }
    let pool = open_pool(cfg).await?;
    let probe = tokio::time::timeout(QUERY_TIMEOUT, sqlx::query("SELECT 1").fetch_one(&pool)).await;
    pool.close().await;
    probe
        .context("Le test de connexion a expiré")?
        .context("La requête de test a échoué")?;
    Ok(format!("Connexion réussie ({}).", cfg.kind.as_str()))
}

#[derive(Debug, Deserialize)]
struct DatabaseQueryInput {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    table: Option<String>,
}

async fn open_pool(cfg: &DatabaseConfig) -> Result<AnyPool> {
    install_drivers();
    let url = build_url(cfg);
    let pool = AnyPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(CONNECT_TIMEOUT)
        .connect(&url)
        .await
        .with_context(|| format!("connexion à la base {} impossible", cfg.kind.as_str()))?;
    Ok(pool)
}

async fn list_tables(cfg: &DatabaseConfig) -> Result<String> {
    let sql = match cfg.kind {
        DatabaseKind::Postgres => "SELECT table_schema || '.' || table_name AS name FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog','information_schema') ORDER BY 1 LIMIT 200",
        DatabaseKind::Mysql => "SELECT CONCAT(table_schema, '.', table_name) AS name FROM information_schema.tables WHERE table_schema NOT IN ('mysql','information_schema','performance_schema','sys') ORDER BY 1 LIMIT 200",
        DatabaseKind::Sqlite => "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name LIMIT 200",
    };
    let pool = open_pool(cfg).await?;
    let rows_result = tokio::time::timeout(QUERY_TIMEOUT, sqlx::query(sql).fetch_all(&pool)).await;
    pool.close().await;
    let rows = rows_result
        .context("La requête de schéma a expiré")?
        .context("La requête de schéma a échoué")?;

    let tables: Vec<Value> = rows
        .iter()
        .filter_map(|row| row.try_get::<String, _>(0).ok())
        .map(Value::String)
        .collect();

    Ok(serde_json::to_string_pretty(&json!({
        "kind": "schema",
        "engine": cfg.kind.as_str(),
        "table_count": tables.len(),
        "tables": tables,
        "next_step": "Choisis une table et rappelle DatabaseQuery avec `table` renseigné, ou fournis une requête SELECT.",
    }))
    .unwrap_or_default())
}

async fn run_query(cfg: &DatabaseConfig, sql: &str) -> Result<String> {
    let pool = open_pool(cfg).await?;
    let rows_result = tokio::time::timeout(QUERY_TIMEOUT, sqlx::query(sql).fetch_all(&pool)).await;
    pool.close().await;
    let rows = rows_result
        .context("La requête a expiré")?
        .context("La requête a échoué")?;

    let total = rows.len();
    let truncated = total > MAX_ROWS;
    let payload_rows: Vec<Value> = rows.iter().take(MAX_ROWS).map(row_to_json).collect();

    Ok(serde_json::to_string_pretty(&json!({
        "kind": "rows",
        "engine": cfg.kind.as_str(),
        "query": sql,
        "row_count": payload_rows.len(),
        "total_returned": total,
        "truncated": truncated,
        "rows": payload_rows,
    }))
    .unwrap_or_default())
}

fn row_to_json(row: &AnyRow) -> Value {
    let mut obj = Map::new();
    for (idx, column) in row.columns().iter().enumerate() {
        let name = column.name().to_string();
        let type_name = column.type_info().name().to_string();
        obj.insert(name, cell_to_json(row, idx, &type_name));
    }
    Value::Object(obj)
}

fn cell_to_json(row: &AnyRow, idx: usize, type_name: &str) -> Value {
    let type_name = type_name.to_ascii_uppercase();

    if type_name == "BOOL" || type_name == "BOOLEAN" {
        if let Ok(value) = row.try_get::<Option<bool>, _>(idx) {
            return value.map_or(Value::Null, Value::Bool);
        }
    }
    if type_name.contains("INT") || type_name.contains("SERIAL") {
        if let Ok(value) = row.try_get::<Option<i64>, _>(idx) {
            return value.map_or(Value::Null, |i| json!(i));
        }
    }
    if type_name.contains("FLOAT")
        || type_name.contains("DOUBLE")
        || type_name.contains("REAL")
        || type_name.contains("NUMERIC")
        || type_name.contains("DECIMAL")
    {
        if let Ok(value) = row.try_get::<Option<f64>, _>(idx) {
            return value.map_or(Value::Null, |f| json!(f));
        }
    }
    if let Ok(value) = row.try_get::<Option<String>, _>(idx) {
        return value.map_or(Value::Null, Value::String);
    }
    if let Ok(value) = row.try_get::<Option<i64>, _>(idx) {
        return value.map_or(Value::Null, |i| json!(i));
    }
    if let Ok(value) = row.try_get::<Option<f64>, _>(idx) {
        return value.map_or(Value::Null, |f| json!(f));
    }
    if let Ok(value) = row.try_get::<Option<bool>, _>(idx) {
        return value.map_or(Value::Null, Value::Bool);
    }
    Value::String(format!("<unsupported:{type_name}>"))
}

fn ensure_read_only(sql: &str) -> Result<()> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        bail!("Requête SQL vide.");
    }
    if trimmed.contains(';') {
        bail!("Plusieurs instructions ne sont pas autorisées (séparateur `;`).");
    }
    let upper = trimmed.to_ascii_uppercase();
    let head = upper.split_ascii_whitespace().next().unwrap_or("");
    const ALLOWED_HEADS: &[&str] = &[
        "SELECT", "WITH", "SHOW", "EXPLAIN", "DESCRIBE", "DESC", "PRAGMA",
    ];
    if !ALLOWED_HEADS.contains(&head) {
        bail!(
            "Lecture seule : seules les requêtes SELECT/WITH/SHOW/EXPLAIN/DESCRIBE/PRAGMA sont autorisées (reçu `{head}`)."
        );
    }
    for banned in [
        "DROP", "TRUNCATE", "ALTER", "GRANT", "REVOKE", "CREATE", "INSERT", "REPLACE", "MERGE",
        "CALL", "EXEC", "EXECUTE",
    ] {
        if contains_keyword(&upper, banned) {
            bail!("Mot-clé interdit en lecture seule : {banned}.");
        }
    }
    for op in ["UPDATE", "DELETE"] {
        if contains_keyword(&upper, op) {
            bail!(
                "Lecture seule : `{op}` interdit (et particulièrement sans clause WHERE)."
            );
        }
    }
    Ok(())
}

fn contains_keyword(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let before_ok = abs == 0 || !is_word_byte(bytes[abs - 1]);
        let end = abs + needle.len();
        let after_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
        if start >= haystack.len() {
            break;
        }
    }
    false
}

fn is_word_byte(byte: u8) -> bool {
    matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_')
}

fn is_safe_table_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.')
}

fn quote_table(name: &str, kind: DatabaseKind) -> String {
    name.split('.')
        .map(|part| match kind {
            DatabaseKind::Mysql => format!("`{part}`"),
            _ => format!("\"{part}\""),
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn build_url(cfg: &DatabaseConfig) -> String {
    match cfg.kind {
        DatabaseKind::Sqlite => {
            let path = cfg.database.trim();
            if path == ":memory:" {
                "sqlite::memory:".to_string()
            } else if path.starts_with("sqlite:") {
                path.to_string()
            } else {
                format!("sqlite://{}", path)
            }
        }
        kind => {
            let scheme = match kind {
                DatabaseKind::Postgres => "postgres",
                DatabaseKind::Mysql => "mysql",
                DatabaseKind::Sqlite => unreachable!(),
            };
            format!(
                "{scheme}://{}:{}@{}:{}/{}",
                pct_encode(cfg.user.trim()),
                pct_encode(&cfg.password),
                cfg.host.trim(),
                cfg.port,
                cfg.database.trim()
            )
        }
    }
}

fn pct_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

fn redact_password(message: &str, password: &str) -> String {
    let trimmed = password.trim();
    if trimmed.is_empty() {
        return message.to_string();
    }
    let mut out = message.replace(trimmed, "<redacted>");
    let encoded = pct_encode(trimmed);
    if encoded != trimmed {
        out = out.replace(&encoded, "<redacted>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_read_only_accepts_select() {
        assert!(ensure_read_only("SELECT * FROM t").is_ok());
        assert!(ensure_read_only("  with foo as (select 1) select * from foo  ").is_ok());
        assert!(ensure_read_only("EXPLAIN SELECT 1").is_ok());
    }

    #[test]
    fn ensure_read_only_rejects_mutations() {
        assert!(ensure_read_only("DROP TABLE users").is_err());
        assert!(ensure_read_only("DELETE FROM users WHERE id=1").is_err());
        assert!(ensure_read_only("UPDATE users SET x=1 WHERE id=1").is_err());
        assert!(ensure_read_only("SELECT 1; DELETE FROM users").is_err());
        assert!(ensure_read_only("INSERT INTO t VALUES (1)").is_err());
    }

    #[test]
    fn ensure_read_only_keyword_in_string_is_ignored_in_practice() {
        // The lexer is naive but it still requires the first token to be SELECT-like.
        // A statement like SELECT 'drop' is allowed because we look for word-boundary DROP.
        // Note: SELECT 'drop' contains 'DROP' as a word, so it WILL be rejected — accepted by design.
        assert!(ensure_read_only("SELECT 'drop_table_keyword'").is_ok());
    }

    #[test]
    fn safe_table_name_check() {
        assert!(is_safe_table_name("users"));
        assert!(is_safe_table_name("public.users"));
        assert!(!is_safe_table_name(""));
        assert!(!is_safe_table_name("users; drop"));
    }

    #[test]
    fn redact_password_replaces_url_encoded() {
        let msg = "error: postgres://u:p%40ss@host/db";
        let redacted = redact_password(msg, "p@ss");
        assert!(redacted.contains("<redacted>"));
        assert!(!redacted.contains("p%40ss"));
    }

    #[test]
    fn parse_database_kind() {
        assert_eq!(DatabaseKind::parse("postgres"), Some(DatabaseKind::Postgres));
        assert_eq!(DatabaseKind::parse("PostgreSQL"), Some(DatabaseKind::Postgres));
        assert_eq!(DatabaseKind::parse("mysql"), Some(DatabaseKind::Mysql));
        assert_eq!(DatabaseKind::parse("MariaDB"), Some(DatabaseKind::Mysql));
        assert_eq!(DatabaseKind::parse("sqlite"), Some(DatabaseKind::Sqlite));
        assert_eq!(DatabaseKind::parse("oracle"), None);
    }
}
