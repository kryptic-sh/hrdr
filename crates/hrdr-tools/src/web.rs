//! Web tools: `fetch` (URL → readable text) and `search` (query → top
//! results). `search` uses a zero-config DuckDuckGo HTML backend by default,
//! or a SearXNG instance when `SEARXNG_URL` is set (a JSON API — more robust).

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;

use crate::{Tool, ToolContext, truncate};

const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64; rv:124.0) Gecko/20100101 Firefox/124.0 hrdr";
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// How many times the context's output byte cap the *raw* fetch body may be,
/// before the download is stopped. HTML is stripped down to a fraction of its
/// source, so a generous multiple leaves enough markup to reduce to `max_output`
/// worth of text while still bounding memory against a hostile/huge response.
const FETCH_BODY_MULTIPLIER: usize = 8;
/// Absolute floor on the raw-body cap, so a small `max_output` still allows a
/// normal page through.
const FETCH_BODY_FLOOR: usize = 256 * 1024;

/// Lazily-initialised, shared HTTP client with a browser-ish UA and a sane
/// timeout. Built once and reused for every web tool call so connection pools
/// and DNS results are shared. A build failure (TLS-backend misconfiguration)
/// is stored as an error string and surfaced per call via [`http_client`], so a
/// broken environment yields a recoverable tool error rather than a panic.
static HTTP_CLIENT: LazyLock<Result<reqwest::Client, String>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| format!("building HTTP client (TLS backend missing or misconfigured): {e}"))
});

/// The shared HTTP client, or a tool error if it failed to build.
fn http_client() -> Result<&'static reqwest::Client> {
    HTTP_CLIENT.as_ref().map_err(|e| anyhow::anyhow!(e.clone()))
}

// ---- fetch ----

pub struct WebFetchTool;

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
    /// Max characters of body text to return (default: the context's byte cap).
    #[serde(default)]
    max_chars: Option<usize>,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "fetch"
    }
    fn description(&self) -> &'static str {
        "Fetch a URL over HTTP(S) and return its content as text. HTML pages are reduced to \
         readable text (scripts/styles/markup stripped). Use for docs, READMEs, API references, \
         or any page whose contents you need."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Absolute http(s) URL to fetch." },
                "max_chars": {
                    "type": "integer",
                    "description": "Optional cap on returned characters."
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: serde_json::Value, ctx: &ToolContext) -> Result<String> {
        let args: FetchArgs = crate::tool_args("fetch", args)?;
        let url = args.url.trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            bail!("url must start with http:// or https://");
        }
        // Block obviously-internal targets: prompt-injected content can point
        // `fetch` at the cloud metadata endpoint or a loopback service to read
        // credentials / pivot (SSRF).
        if is_blocked_host(url) {
            bail!("refusing to fetch {url}: internal/loopback host is blocked");
        }
        let resp = http_client()?.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            bail!("HTTP {status} fetching {url}");
        }
        let is_html = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|c| c.contains("html"))
            .unwrap_or(false);
        // Stream the body with a hard byte cap instead of buffering an unbounded
        // `resp.text()` — a hostile server could otherwise stream gigabytes and
        // OOM the process.
        let raw_cap = ctx
            .max_output
            .saturating_mul(FETCH_BODY_MULTIPLIER)
            .max(FETCH_BODY_FLOOR);
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut body_truncated = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("reading response body")?;
            if push_capped(&mut buf, &chunk, raw_cap) {
                body_truncated = true;
                break;
            }
        }
        let body = String::from_utf8_lossy(&buf).into_owned();
        let text = if is_html || looks_like_html(&body) {
            strip_html(&body)
        } else {
            body
        };
        let cap = args.max_chars.unwrap_or(ctx.max_output);
        let mut out = format!("URL: {url}\n\n{}", truncate(text.trim(), cap));
        if body_truncated {
            out.push_str("\n\n… [response body truncated at the fetch size cap]");
        }
        Ok(out)
    }
}

// ---- search ----

pub struct WebSearchTool;

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    max_results: Option<usize>,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn read_only(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "search"
    }
    fn description(&self) -> &'static str {
        "Search the web and return the top results (title, URL, snippet). Follow up with \
         fetch to read a result in full. Uses DuckDuckGo by default, or a SearXNG instance \
         if SEARXNG_URL is set."
    }
    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query." },
                "max_results": {
                    "type": "integer",
                    "description": "Number of results to return (default 5, max 10)."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value, _ctx: &ToolContext) -> Result<String> {
        let args: SearchArgs = crate::tool_args("search", args)?;
        let query = args.query.trim();
        if query.is_empty() {
            bail!("query must not be empty");
        }
        let n = args.max_results.unwrap_or(5).clamp(1, 10);

        let results = if let Ok(base) = std::env::var("SEARXNG_URL") {
            searxng_search(&base, query, n).await?
        } else {
            ddg_search(query, n).await?
        };

        if results.is_empty() {
            return Ok(format!(
                "No results for {query:?}. (For more reliable search, run a SearXNG instance and \
                 set SEARXNG_URL.)"
            ));
        }
        let mut out = format!("Search results for {query:?}:\n");
        for (i, (title, url, snippet)) in results.iter().enumerate() {
            out.push_str(&format!("\n{}. {title}\n   {url}\n", i + 1));
            if !snippet.is_empty() {
                out.push_str(&format!("   {snippet}\n"));
            }
        }
        Ok(out)
    }
}

/// One search hit: `(title, url, snippet)`.
type Hit = (String, String, String);

/// Query a SearXNG instance's JSON API.
async fn searxng_search(base: &str, query: &str, n: usize) -> Result<Vec<Hit>> {
    let url = format!(
        "{}/search?q={}&format=json",
        base.trim_end_matches('/'),
        percent_encode(query)
    );
    let resp = http_client()?.get(&url).send().await?;
    if !resp.status().is_success() {
        bail!("SearXNG HTTP {} from {base}", resp.status());
    }
    let v: serde_json::Value = resp.json().await?;
    let mut hits = Vec::new();
    if let Some(arr) = v.get("results").and_then(|r| r.as_array()) {
        for r in arr.iter().take(n) {
            let title = r.get("title").and_then(|x| x.as_str()).unwrap_or("");
            let url = r.get("url").and_then(|x| x.as_str()).unwrap_or("");
            let snippet = r.get("content").and_then(|x| x.as_str()).unwrap_or("");
            if !url.is_empty() {
                hits.push((title.to_string(), url.to_string(), collapse_ws(snippet)));
            }
        }
    }
    Ok(hits)
}

/// Query DuckDuckGo's HTML endpoint and scrape the result list.
async fn ddg_search(query: &str, n: usize) -> Result<Vec<Hit>> {
    let url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        percent_encode(query)
    );
    let resp = http_client()?.get(&url).send().await?;
    if !resp.status().is_success() {
        bail!("DuckDuckGo HTTP {}", resp.status());
    }
    let html = resp.text().await?;
    Ok(parse_ddg(&html, n))
}

/// Extract `(title, url, snippet)` triples from DuckDuckGo HTML.
fn parse_ddg(html: &str, n: usize) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut cursor = 0;
    while hits.len() < n {
        let Some(rel) = html[cursor..].find("result__a") else {
            break;
        };
        let marker = cursor + rel;
        // The opening <a …> tag this class belongs to.
        let Some(tag_open) = html[..marker].rfind("<a") else {
            cursor = marker + "result__a".len();
            continue;
        };
        let Some(gt) = html[tag_open..].find('>') else {
            break;
        };
        let tag_end = tag_open + gt;
        let href = attr_value(&html[tag_open..tag_end], "href").unwrap_or_default();
        // Title text up to the closing </a>.
        let after = tag_end + 1;
        let Some(close) = html[after..].find("</a>") else {
            break;
        };
        let title = collapse_ws(&decode_entities(&strip_tags(&html[after..after + close])));
        let url = clean_ddg_url(&href);

        // Snippet: the next result__snippet anchor's text — but only within
        // this result's block (bounded by the next result link), else a
        // snippet-less result would steal the following result's snippet.
        let block_end = html[after..]
            .find("result__a")
            .map(|r| after + r)
            .unwrap_or(html.len());
        let snippet = html[after..block_end]
            .find("result__snippet")
            .and_then(|srel| {
                let s = after + srel;
                let sgt = html[s..].find('>')? + s + 1;
                let send = html[sgt..].find("</a>")? + sgt;
                Some(collapse_ws(&decode_entities(&strip_tags(&html[sgt..send]))))
            })
            .unwrap_or_default();

        if !url.is_empty() && !title.is_empty() {
            hits.push((title, url, snippet));
        }
        cursor = after + close + 4;
    }
    hits
}

/// DuckDuckGo wraps result links in a `…/l/?uddg=<encoded>` redirect; unwrap it.
fn clean_ddg_url(href: &str) -> String {
    if let Some(idx) = href.find("uddg=") {
        let rest = &href[idx + 5..];
        let enc = rest.split('&').next().unwrap_or(rest);
        return percent_decode(enc);
    }
    if let Some(stripped) = href.strip_prefix("//") {
        return format!("https://{stripped}");
    }
    href.to_string()
}

/// Append `chunk` to `buf` without letting it exceed `cap` bytes. Returns `true`
/// when the cap is reached (the caller stops reading) — the streaming guard that
/// bounds `fetch`'s response body. Pure, so the cap logic is unit-testable
/// without a live server.
fn push_capped(buf: &mut Vec<u8>, chunk: &[u8], cap: usize) -> bool {
    let remaining = cap.saturating_sub(buf.len());
    if chunk.len() >= remaining {
        buf.extend_from_slice(&chunk[..remaining]);
        true
    } else {
        buf.extend_from_slice(chunk);
        false
    }
}

/// Whether `url`'s host is an internal/loopback target `fetch` should refuse
/// (SSRF guard): localhost, loopback IPs, `0.0.0.0`, and the link-local cloud
/// metadata endpoint. A URL that doesn't parse is not blocked here (the caller
/// already enforced an `http(s)` scheme).
fn is_blocked_host(url: &str) -> bool {
    match reqwest::Url::parse(url) {
        Ok(u) => u.host_str().is_some_and(is_internal_host),
        Err(_) => false,
    }
}

/// The host-name test behind [`is_blocked_host`], split out for unit tests.
fn is_internal_host(host: &str) -> bool {
    let h = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    h == "localhost"
        || h.ends_with(".localhost")
        || h == "0.0.0.0"
        || h == "::1"
        || h == "169.254.169.254" // cloud instance metadata
        || h.starts_with("127.")
}

// ---- small HTML helpers (no extra dependencies) ----

fn looks_like_html(s: &str) -> bool {
    let head = s[..crate::floor_char_boundary(s, 512)].to_ascii_lowercase();
    head.contains("<html") || head.contains("<!doctype html") || head.contains("<body")
}

/// Reduce an HTML document to readable plain text.
fn strip_html(html: &str) -> String {
    let mut s = remove_block(html, "script");
    s = remove_block(&s, "style");
    // Turn common block-closing tags into line breaks so structure survives.
    for tag in [
        "</p>",
        "<br>",
        "<br/>",
        "<br />",
        "</div>",
        "</li>",
        "</tr>",
        "</h1>",
        "</h2>",
        "</h3>",
        "</h4>",
        "</h5>",
        "</h6>",
        "</section>",
        "</article>",
        "</header>",
        "</footer>",
    ] {
        s = replace_ci(&s, tag, "\n");
    }
    let text = decode_entities(&strip_tags(&s));
    // Collapse runs of blank lines and trim each line.
    let mut out = String::new();
    let mut blank = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            if !blank {
                out.push('\n');
            }
            blank = true;
        } else {
            out.push_str(line);
            out.push('\n');
            blank = false;
        }
    }
    out.trim().to_string()
}

/// Remove `<tag …>…</tag>` blocks (case-insensitive), e.g. script/style.
fn remove_block(input: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if let Some(rel) = lower[i..].find(&open) {
            let start = i + rel;
            out.push_str(&input[i..start]);
            match lower[start..].find(&close) {
                Some(crel) => i = start + crel + close.len(),
                None => break,
            }
        } else {
            out.push_str(&input[i..]);
            break;
        }
    }
    out
}

/// Drop everything between `<` and `>`.
fn strip_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;
    for c in input.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Case-insensitive replace of `needle` with `to`.
fn replace_ci(haystack: &str, needle: &str, to: &str) -> String {
    let lower = haystack.to_ascii_lowercase();
    let nl = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if let Some(rel) = lower[i..].find(&nl) {
            let start = i + rel;
            out.push_str(&haystack[i..start]);
            out.push_str(to);
            i = start + needle.len();
        } else {
            out.push_str(&haystack[i..]);
            break;
        }
    }
    out
}

/// Decode the handful of HTML entities that matter for readable text.
fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
}

/// The value of an HTML attribute within a single tag string (handles `"` / `'`).
fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let key = format!("{attr}=");
    let start = tag.find(&key)? + key.len();
    let rest = &tag[start..];
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let end = rest[1..].find(quote)?;
        Some(rest[1..1 + end].to_string())
    } else {
        let end = rest.find([' ', '>']).unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

/// Collapse all runs of whitespace to single spaces and trim.
fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Percent-encode a query string (RFC 3986 unreserved kept; space → `%20`).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-decode, also turning `+` into space.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_html_to_text() {
        let html = "<html><head><style>x{}</style><script>bad()</script></head>\
                    <body><h1>Title</h1><p>Hello &amp; welcome</p><p>Line two</p></body></html>";
        let text = strip_html(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello & welcome"));
        assert!(text.contains("Line two"));
        assert!(!text.contains("bad()"));
        assert!(!text.contains("x{}"));
    }

    #[test]
    fn unwraps_ddg_redirect() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa&rut=xyz";
        assert_eq!(clean_ddg_url(href), "https://example.com/a");
        assert_eq!(clean_ddg_url("//example.com/b"), "https://example.com/b");
    }

    #[test]
    fn percent_roundtrip() {
        assert_eq!(percent_encode("a b&c"), "a%20b%26c");
        assert_eq!(percent_decode("https%3A%2F%2Fx.com"), "https://x.com");
    }

    #[test]
    fn push_capped_bounds_body() {
        let mut buf = Vec::new();
        assert!(!push_capped(&mut buf, b"hello", 10));
        // This chunk overflows the cap: only the first 5 bytes are kept.
        assert!(push_capped(&mut buf, b"world!!!", 10));
        assert_eq!(buf, b"helloworld");
    }

    #[test]
    fn push_capped_exact_fill_signals_stop() {
        let mut buf = Vec::new();
        assert!(push_capped(&mut buf, b"0123456789", 10));
        assert_eq!(buf.len(), 10);
    }

    #[test]
    fn blocks_internal_hosts() {
        assert!(is_blocked_host("http://localhost:8080/x"));
        assert!(is_blocked_host("http://127.0.0.1/x"));
        assert!(is_blocked_host("http://127.5.5.5/x"));
        assert!(is_blocked_host("http://[::1]/x"));
        assert!(is_blocked_host("http://169.254.169.254/latest/meta-data"));
        assert!(is_blocked_host("http://app.localhost/x"));
        assert!(!is_blocked_host("https://example.com/x"));
        assert!(!is_blocked_host("https://8.8.8.8/x"));
    }

    #[test]
    fn extracts_attr() {
        assert_eq!(
            attr_value("<a class=\"result__a\" href=\"http://x\">", "href"),
            Some("http://x".to_string())
        );
    }
}
