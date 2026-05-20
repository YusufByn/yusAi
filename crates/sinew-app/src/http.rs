use std::{collections::BTreeMap, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE},
    Method, Url,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sinew_core::ToolDescriptor;

use crate::tool_run::ToolRunResult;

pub const HTTP_REQUEST_TOOL_NAME: &str = "HttpRequest";

const USER_AGENT: &str = "sinew-http/0.1";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 120_000;
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const TOOL_OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, Default)]
pub struct HttpRequestTool;

impl HttpRequestTool {
    pub fn new() -> Self {
        Self
    }

    pub fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: HTTP_REQUEST_TOOL_NAME.into(),
            description: "Send an HTTP request and read the structured response. Use this to test an endpoint you just wrote (typically against a local dev server like http://localhost:3000), validate a third-party API, or reproduce a curl call without quoting hell. Returns status code, response headers, and body (auto pretty-printed when JSON). Non-2xx responses are returned as data so you can read the error payload and fix the code.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "Absolute URL (http:// or https://). Localhost and private addresses are allowed."
                    },
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"],
                        "description": "HTTP method. Defaults to GET."
                    },
                    "headers": {
                        "type": "object",
                        "description": "Request headers as { name: value }. Authorization, Content-Type, etc.",
                        "additionalProperties": { "type": "string" }
                    },
                    "query": {
                        "type": "object",
                        "description": "Query string parameters appended to the URL.",
                        "additionalProperties": true
                    },
                    "json": {
                        "description": "Request body sent as application/json. Pass any JSON value (object, array, string, number)."
                    },
                    "form": {
                        "type": "object",
                        "description": "Request body sent as application/x-www-form-urlencoded.",
                        "additionalProperties": true
                    },
                    "body": {
                        "type": "string",
                        "description": "Raw request body as a string. Set Content-Type via headers when using this."
                    },
                    "timeout_ms": {
                        "type": "number",
                        "description": "Request timeout in milliseconds. Defaults to 30000, max 120000."
                    },
                    "follow_redirects": {
                        "type": "boolean",
                        "description": "Whether to follow 3xx redirects automatically. Defaults to true."
                    }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        }
    }

    pub async fn run(&self, input: Value) -> ToolRunResult {
        match self.execute(input).await {
            Ok(output) => ToolRunResult::ok(output, Vec::new()),
            Err(err) => ToolRunResult::err(err.to_string(), Vec::new()),
        }
    }

    async fn execute(&self, input: Value) -> Result<String> {
        let parsed: HttpRequestInput = serde_json::from_value(input)
            .map_err(|err| anyhow!("invalid HttpRequest input: {err}"))?;
        let request_plan = parsed.into_plan()?;

        let timeout = Duration::from_millis(request_plan.timeout_ms);
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(USER_AGENT)
            .redirect(if request_plan.follow_redirects {
                reqwest::redirect::Policy::limited(10)
            } else {
                reqwest::redirect::Policy::none()
            })
            .build()
            .context("unable to build HTTP client")?;

        let mut builder = client
            .request(request_plan.method.clone(), request_plan.url.clone())
            .headers(request_plan.headers.clone());

        if let Some(body) = &request_plan.body {
            builder = match body {
                RequestBody::Json(value) => builder.json(value),
                RequestBody::Form(pairs) => builder.form(pairs),
                RequestBody::Raw(text) => builder.body(text.clone()),
            };
        }

        let started = std::time::Instant::now();
        let response = builder.send().await.context("HTTP request failed")?;
        let elapsed = started.elapsed();

        let status = response.status();
        let final_url = response.url().to_string();
        let response_headers = response.headers().clone();
        let content_type = response_headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();

        let (body_bytes, truncated) = collect_response_bytes(response, MAX_RESPONSE_BYTES).await?;
        let body_text = format_response_body(&body_bytes, &content_type);

        let mut output = String::new();
        output.push_str(&format!(
            "status: {} {}\n",
            status.as_u16(),
            status.canonical_reason().unwrap_or("")
        ));
        output.push_str(&format!("method: {}\n", request_plan.method));
        output.push_str(&format!("url: {final_url}\n"));
        output.push_str(&format!("duration_ms: {}\n", elapsed.as_millis()));
        if !content_type.is_empty() {
            output.push_str(&format!("content-type: {content_type}\n"));
        }
        output.push_str(&format!("response_bytes: {}\n", body_bytes.len()));

        let formatted_headers = format_response_headers(&response_headers);
        if !formatted_headers.is_empty() {
            output.push_str("\nresponse_headers:\n");
            output.push_str(&formatted_headers);
        }

        output.push_str("\nresponse_body:\n");
        if body_text.is_empty() {
            output.push_str("(empty)\n");
        } else {
            output.push_str(&body_text);
            if !body_text.ends_with('\n') {
                output.push('\n');
            }
        }
        if truncated {
            output.push_str("\n[Response body truncated]\n");
        }

        Ok(clip_with_notice(output, TOOL_OUTPUT_LIMIT))
    }
}

#[derive(Debug, Deserialize)]
struct HttpRequestInput {
    url: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    query: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    json: Option<Value>,
    #[serde(default)]
    form: Option<BTreeMap<String, Value>>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    follow_redirects: Option<bool>,
}

struct RequestPlan {
    method: Method,
    url: Url,
    headers: HeaderMap,
    body: Option<RequestBody>,
    timeout_ms: u64,
    follow_redirects: bool,
}

enum RequestBody {
    Json(Value),
    Form(Vec<(String, String)>),
    Raw(String),
}

impl HttpRequestInput {
    fn into_plan(self) -> Result<RequestPlan> {
        let raw_url = self.url.trim();
        if raw_url.is_empty() {
            bail!("url is required");
        }
        let mut url = Url::parse(raw_url).context("invalid url")?;
        match url.scheme() {
            "http" | "https" => {}
            other => bail!("unsupported url scheme `{other}` (only http and https are allowed)"),
        }

        if let Some(query) = self.query {
            if !query.is_empty() {
                let mut pairs = url.query_pairs_mut();
                for (key, value) in &query {
                    pairs.append_pair(key, &value_to_string(value));
                }
                drop(pairs);
            }
        }

        let method_text = self
            .method
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("GET")
            .to_ascii_uppercase();
        let method = Method::from_bytes(method_text.as_bytes())
            .with_context(|| format!("invalid HTTP method `{method_text}`"))?;

        let mut headers = HeaderMap::new();
        if let Some(input_headers) = self.headers {
            for (name, value) in input_headers {
                let header_name = HeaderName::from_bytes(name.as_bytes())
                    .with_context(|| format!("invalid header name `{name}`"))?;
                let header_value_text = value_to_string(&value);
                let header_value = HeaderValue::from_str(&header_value_text)
                    .with_context(|| format!("invalid value for header `{name}`"))?;
                headers.insert(header_name, header_value);
            }
        }

        let bodies_set = [
            self.json.is_some(),
            self.form.is_some(),
            self.body.is_some(),
        ]
        .iter()
        .filter(|v| **v)
        .count();
        if bodies_set > 1 {
            bail!("only one of `json`, `form`, or `body` may be set");
        }

        let body = if let Some(value) = self.json {
            Some(RequestBody::Json(value))
        } else if let Some(form) = self.form {
            let pairs = form
                .into_iter()
                .map(|(key, value)| (key, value_to_string(&value)))
                .collect();
            Some(RequestBody::Form(pairs))
        } else if let Some(text) = self.body {
            Some(RequestBody::Raw(text))
        } else {
            None
        };

        let timeout_ms = self
            .timeout_ms
            .map(|value| value.clamp(1, MAX_TIMEOUT_MS))
            .unwrap_or(DEFAULT_TIMEOUT_MS);
        let follow_redirects = self.follow_redirects.unwrap_or(true);

        Ok(RequestPlan {
            method,
            url,
            headers,
            body,
            timeout_ms,
            follow_redirects,
        })
    }
}

fn value_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Number(num) => num.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn format_response_headers(headers: &HeaderMap) -> String {
    const HIDDEN_HEADERS: &[&str] = &["set-cookie"];
    let mut lines = Vec::new();
    for (name, value) in headers.iter() {
        let name_str = name.as_str();
        if HIDDEN_HEADERS.contains(&name_str.to_ascii_lowercase().as_str()) {
            lines.push(format!("  {name_str}: [hidden]"));
            continue;
        }
        let value_str = value.to_str().unwrap_or("[non-utf8]");
        lines.push(format!("  {name_str}: {value_str}"));
    }
    if lines.is_empty() {
        String::new()
    } else {
        let mut out = lines.join("\n");
        out.push('\n');
        out
    }
}

fn format_response_body(bytes: &[u8], content_type: &str) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let lowered = content_type.to_ascii_lowercase();
    let looks_textual = lowered.contains("json")
        || lowered.contains("text")
        || lowered.contains("xml")
        || lowered.contains("javascript")
        || lowered.contains("html")
        || lowered.contains("form-urlencoded");

    if looks_textual || content_type.is_empty() {
        let text = String::from_utf8_lossy(bytes).into_owned();
        if lowered.contains("json") || (content_type.is_empty() && looks_like_json(&text)) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
                if let Ok(pretty) = serde_json::to_string_pretty(&parsed) {
                    return pretty;
                }
            }
        }
        text
    } else {
        format!("[binary response · {} bytes · {content_type}]", bytes.len())
    }
}

fn looks_like_json(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with('{') || trimmed.starts_with('[')
}

async fn collect_response_bytes(
    response: reqwest::Response,
    byte_limit: usize,
) -> Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("unable to read response body")?;
        let remaining = byte_limit.saturating_sub(bytes.len());
        if chunk.len() > remaining {
            bytes.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() >= byte_limit {
            truncated = true;
            break;
        }
    }
    Ok((bytes, truncated))
}

fn clip_with_notice(text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text;
    }
    let mut clipped: String = text.chars().take(limit).collect();
    clipped.push_str("\n\n[Output truncated]");
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_url() {
        let input = json!({});
        let parsed: Result<HttpRequestInput, _> = serde_json::from_value(input);
        assert!(parsed.is_err());
    }

    #[test]
    fn builds_plan_with_defaults() {
        let input: HttpRequestInput =
            serde_json::from_value(json!({ "url": "https://example.com" })).unwrap();
        let plan = input.into_plan().unwrap();
        assert_eq!(plan.method, Method::GET);
        assert_eq!(plan.url.as_str(), "https://example.com/");
        assert_eq!(plan.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert!(plan.follow_redirects);
        assert!(plan.body.is_none());
    }

    #[test]
    fn rejects_non_http_scheme() {
        let input: HttpRequestInput =
            serde_json::from_value(json!({ "url": "file:///etc/passwd" })).unwrap();
        assert!(input.into_plan().is_err());
    }

    #[test]
    fn rejects_multiple_bodies() {
        let input: HttpRequestInput = serde_json::from_value(json!({
            "url": "https://example.com",
            "json": { "a": 1 },
            "body": "raw"
        }))
        .unwrap();
        assert!(input.into_plan().is_err());
    }

    #[test]
    fn applies_query_and_headers() {
        let input: HttpRequestInput = serde_json::from_value(json!({
            "url": "https://api.example.com/path",
            "method": "post",
            "headers": { "X-Token": "abc" },
            "query": { "limit": 10, "active": true },
            "json": { "name": "n" }
        }))
        .unwrap();
        let plan = input.into_plan().unwrap();
        assert_eq!(plan.method, Method::POST);
        let pairs: Vec<_> = plan.url.query_pairs().collect();
        assert!(pairs.iter().any(|(k, v)| k == "limit" && v == "10"));
        assert!(pairs.iter().any(|(k, v)| k == "active" && v == "true"));
        assert_eq!(
            plan.headers.get("x-token").and_then(|v| v.to_str().ok()),
            Some("abc")
        );
        assert!(matches!(plan.body, Some(RequestBody::Json(_))));
    }

    #[test]
    fn clamps_timeout() {
        let input: HttpRequestInput = serde_json::from_value(json!({
            "url": "https://example.com",
            "timeout_ms": 999_999
        }))
        .unwrap();
        let plan = input.into_plan().unwrap();
        assert_eq!(plan.timeout_ms, MAX_TIMEOUT_MS);
    }

    #[test]
    fn pretty_prints_json_body() {
        let bytes = b"{\"a\":1,\"b\":2}";
        let out = format_response_body(bytes, "application/json");
        assert!(out.contains("\"a\""));
        assert!(out.contains('\n'));
    }

    #[test]
    fn marks_binary_response() {
        let out = format_response_body(&[0u8, 1, 2, 3], "image/png");
        assert!(out.contains("binary response"));
    }
}
