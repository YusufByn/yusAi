use std::time::Duration;

use anyhow::{bail, Context, Result};
use reqwest::{
    header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE},
    StatusCode,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sinew_core::ToolDescriptor;

use crate::tool_run::ToolRunResult;

const USER_AGENT: &str = "sinew/0.1";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ROWS: usize = 50;
const NOT_CONFIGURED_MESSAGE: &str =
    "Supabase non configuré — renseigne ton URL et ta clé dans Settings > Data Sources.";
pub const SUPABASE_QUERY_TOOL_NAME: &str = "SupabaseQuery";
pub const SUPABASE_QUERY_TOOL_DEFAULT_DESCRIPTION: &str = "Interroge la base de données Supabase configurée dans Settings > Data Sources. Fournis une question en langage naturel et optionnellement un nom de table. Retourne des résultats en JSON. Vérifie toujours le schéma en premier si la table est inconnue. N'expose jamais les credentials dans les résultats.";

#[derive(Debug, Clone, Default)]
pub struct SupabaseQueryTool {
    http: reqwest::Client,
    url: Option<String>,
    key: Option<String>,
}

impl SupabaseQueryTool {
    pub fn new() -> Self {
        Self::with_credentials(None, None)
    }

    pub fn with_credentials(url: Option<String>, key: Option<String>) -> Self {
        Self {
            http: build_http_client(),
            url: url
                .map(|value| value.trim().trim_end_matches('/').to_string())
                .filter(|value| !value.is_empty()),
            key: key
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
        }
    }

    pub fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: SUPABASE_QUERY_TOOL_NAME.to_string(),
            description: SUPABASE_QUERY_TOOL_DEFAULT_DESCRIPTION.to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Question en langage naturel décrivant ce que tu veux obtenir. Tu peux aussi y inclure un filtre PostgREST (ex: `status=eq.open&limit=10`)."
                    },
                    "table": {
                        "type": "string",
                        "description": "Nom de la table Supabase à interroger. Si omis, l'outil renvoie le schéma disponible."
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    pub async fn run(&self, input: Value) -> ToolRunResult {
        let parsed: SupabaseQueryInput = match serde_json::from_value(input) {
            Ok(value) => value,
            Err(err) => {
                return ToolRunResult::err(
                    format!("invalid SupabaseQuery input: {err}"),
                    Vec::new(),
                )
            }
        };

        let query = parsed.query.unwrap_or_default();
        let table = parsed
            .table
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        let Some(url) = self.url.as_deref() else {
            return ToolRunResult::ok(NOT_CONFIGURED_MESSAGE.to_string(), Vec::new());
        };
        let Some(key) = self.key.as_deref() else {
            return ToolRunResult::ok(NOT_CONFIGURED_MESSAGE.to_string(), Vec::new());
        };

        let result = match table.as_deref() {
            Some(table) => self.query_table(url, key, table, query.trim()).await,
            None => self.fetch_schema(url, key).await,
        };

        match result {
            Ok(output) => ToolRunResult::ok(output, Vec::new()),
            Err(err) => ToolRunResult::err(redact_credentials(&err.to_string(), key), Vec::new()),
        }
    }

    async fn fetch_schema(&self, url: &str, key: &str) -> Result<String> {
        let endpoint = format!("{url}/rest/v1/");
        let response = self
            .http
            .get(&endpoint)
            .headers(supabase_headers(key)?)
            .send()
            .await
            .context("Supabase schema request failed")?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("unable to read Supabase schema response")?;

        if !status.is_success() {
            return Err(supabase_error("schema", status, &body));
        }

        let summary = match serde_json::from_str::<Value>(&body) {
            Ok(value) => summarize_schema(&value),
            Err(_) => json!({
                "warning": "schema response was not JSON",
                "raw_preview": clip(&body, 1_500),
            }),
        };

        Ok(serde_json::to_string_pretty(&json!({
            "kind": "schema",
            "summary": summary,
            "next_step": "Choisis une table puis rappelle SupabaseQuery avec `table` renseigné.",
        }))
        .unwrap_or_else(|_| body))
    }

    async fn query_table(
        &self,
        url: &str,
        key: &str,
        table: &str,
        query: &str,
    ) -> Result<String> {
        if !is_safe_table_name(table) {
            bail!("invalid table name `{table}`; expected letters, digits, underscores or dot");
        }

        let mut endpoint = format!("{url}/rest/v1/{table}");
        let extra = postgrest_query_extension(query);
        let limit = MAX_ROWS;
        let mut params = format!("select=*&limit={limit}");
        if let Some(extra) = extra {
            params.push('&');
            params.push_str(&extra);
        }
        endpoint.push('?');
        endpoint.push_str(&params);

        let mut headers = supabase_headers(key)?;
        headers.insert(
            "Prefer",
            HeaderValue::from_static("count=exact"),
        );

        let response = self
            .http
            .get(&endpoint)
            .headers(headers)
            .send()
            .await
            .context("Supabase query request failed")?;

        let status = response.status();
        let content_range = response
            .headers()
            .get("content-range")
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_string());
        let body = response
            .text()
            .await
            .context("unable to read Supabase query response")?;

        if !status.is_success() {
            return Err(supabase_error(table, status, &body));
        }

        let rows = match serde_json::from_str::<Value>(&body) {
            Ok(Value::Array(rows)) => rows,
            Ok(other) => vec![other],
            Err(err) => bail!("Supabase a renvoyé une réponse non-JSON: {err}"),
        };

        let total = parse_total_from_content_range(content_range.as_deref())
            .unwrap_or(rows.len());
        let truncated = rows.len() >= MAX_ROWS && total > rows.len();

        let payload = json!({
            "kind": "rows",
            "table": table,
            "query": query,
            "row_count": rows.len(),
            "total_count": total,
            "truncated": truncated,
            "rows": rows,
        });

        Ok(serde_json::to_string_pretty(&payload).unwrap_or_else(|_| body))
    }
}

pub async fn test_supabase_connection(url: &str, key: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    let key = key.trim();
    if url.is_empty() {
        bail!("L'URL Supabase est obligatoire");
    }
    if key.is_empty() {
        bail!("La clé API Supabase est obligatoire");
    }

    let client = build_http_client();
    let endpoint = format!("{url}/rest/v1/");
    let response = client
        .get(&endpoint)
        .headers(supabase_headers(key)?)
        .send()
        .await
        .context("Impossible de joindre Supabase")?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        bail!(
            "Supabase a répondu avec un statut {status}: {}",
            redact_credentials(&clip(&body, 400), key)
        );
    }

    Ok(format!("Connexion réussie (statut {status})."))
}

#[derive(Debug, Deserialize)]
struct SupabaseQueryInput {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    table: Option<String>,
}

fn build_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .expect("supabase http client")
}

fn supabase_headers(key: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    let bearer = HeaderValue::from_str(&format!("Bearer {key}"))
        .map_err(|_| anyhow::anyhow!("invalid Supabase key"))?;
    let api_key = HeaderValue::from_str(key)
        .map_err(|_| anyhow::anyhow!("invalid Supabase key"))?;
    headers.insert(AUTHORIZATION, bearer);
    headers.insert("apikey", api_key);
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

fn supabase_error(scope: &str, status: StatusCode, body: &str) -> anyhow::Error {
    let preview = clip(body, 600);
    let hint = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            "Droits insuffisants ou clé invalide."
        }
        StatusCode::NOT_FOUND => "Ressource introuvable (table ou endpoint inconnu).",
        StatusCode::BAD_REQUEST => "Requête PostgREST invalide.",
        _ => "Réponse en erreur de Supabase.",
    };
    anyhow::anyhow!("{hint} (scope={scope}, status={status}): {preview}")
}

fn summarize_schema(value: &Value) -> Value {
    let definitions = value
        .get("definitions")
        .and_then(|value| value.as_object());
    if let Some(definitions) = definitions {
        let mut tables = Vec::new();
        for (name, definition) in definitions.iter().take(80) {
            let columns = definition
                .get("properties")
                .and_then(|value| value.as_object())
                .map(|columns| {
                    columns
                        .iter()
                        .take(40)
                        .map(|(column_name, column)| {
                            let kind = column
                                .get("type")
                                .and_then(|value| value.as_str())
                                .unwrap_or("unknown");
                            json!({ "name": column_name, "type": kind })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            tables.push(json!({
                "name": name,
                "columns": columns,
            }));
        }
        let total = definitions.len();
        let returned = tables.len();
        return json!({
            "total_tables": total,
            "returned_tables": returned,
            "tables": tables,
        });
    }

    let paths = value
        .get("paths")
        .and_then(|value| value.as_object())
        .map(|paths| {
            paths
                .keys()
                .filter_map(|key| key.strip_prefix('/'))
                .filter(|key| !key.is_empty() && !key.starts_with("rpc/"))
                .take(80)
                .map(|key| json!({ "name": key }))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({ "tables": paths })
}

fn parse_total_from_content_range(value: Option<&str>) -> Option<usize> {
    let value = value?;
    let total = value.rsplit('/').next()?;
    if total == "*" {
        return None;
    }
    total.parse::<usize>().ok()
}

fn is_safe_table_name(table: &str) -> bool {
    if table.is_empty() || table.len() > 128 {
        return false;
    }
    table
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '.')
}

fn postgrest_query_extension(query: &str) -> Option<String> {
    let trimmed = query.trim().trim_start_matches('?').trim();
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed.contains('=') {
        return None;
    }
    // Drop any user-supplied select=/limit= so the tool stays bounded.
    let filtered = trimmed
        .split('&')
        .map(|segment| segment.trim())
        .filter(|segment| !segment.is_empty())
        .filter(|segment| {
            let lower = segment.to_ascii_lowercase();
            !lower.starts_with("select=") && !lower.starts_with("limit=")
        })
        .collect::<Vec<_>>()
        .join("&");
    if filtered.is_empty() {
        None
    } else {
        Some(filtered)
    }
}

fn clip(value: &str, max: usize) -> String {
    if value.len() <= max {
        value.to_string()
    } else {
        let mut end = max;
        while !value.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &value[..end])
    }
}

fn redact_credentials(value: &str, key: &str) -> String {
    let mut output = value.to_string();
    if !key.is_empty() {
        output = output.replace(key, "<redacted>");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgrest_extension_drops_unsafe_params() {
        assert_eq!(
            postgrest_query_extension("status=eq.open&select=id&limit=100"),
            Some("status=eq.open".to_string())
        );
    }

    #[test]
    fn natural_language_query_is_ignored() {
        assert_eq!(
            postgrest_query_extension("liste les utilisateurs actifs"),
            None
        );
    }

    #[test]
    fn rejects_unsafe_table_names() {
        assert!(is_safe_table_name("users"));
        assert!(is_safe_table_name("public.users"));
        assert!(!is_safe_table_name("users; drop"));
        assert!(!is_safe_table_name(""));
    }

    #[test]
    fn parses_total_from_content_range() {
        assert_eq!(parse_total_from_content_range(Some("0-49/1234")), Some(1234));
        assert_eq!(parse_total_from_content_range(Some("0-49/*")), None);
        assert_eq!(parse_total_from_content_range(None), None);
    }

    #[test]
    fn redacts_secret_in_messages() {
        let redacted = redact_credentials("token=secret123 was rejected", "secret123");
        assert_eq!(redacted, "token=<redacted> was rejected");
    }
}
