use std::sync::OnceLock;

use axum::http::HeaderMap;
use chrono::DateTime;
use regex::{Captures, Regex};
use rusqlite::params;

use crate::state::AppState;

/// GitHub owner/repo segments: no separators, no `.`/`..`, no whitespace.
fn is_valid_slug_part(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 100
        && part != "."
        && part != ".."
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn parse_github_slug(raw: &str) -> Option<String> {
    let mut s = raw.trim().trim_end_matches('/').to_string();
    if let Some(idx) = s.find("github.com/") {
        s = s[idx + "github.com/".len()..].to_string();
    }
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() >= 2 {
        let owner = parts[0];
        let repo = parts[1].trim_end_matches(".git");
        if is_valid_slug_part(owner) && is_valid_slug_part(repo) {
            return Some(format!("{}/{}", owner, repo));
        }
    }
    None
}
pub fn update_rate_limit_from_headers(state: &AppState, user_id: i64, headers: &HeaderMap) {
    let rem = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    let lim = headers
        .get("x-ratelimit-limit")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(|ts| DateTime::from_timestamp(ts, 0).map(|dt| dt.to_rfc3339()));

    if rem.is_some() || lim.is_some() {
        if let Ok(conn) = state.db() {
            let _ = conn.execute(
                "UPDATE user_settings SET rate_limit_remaining = COALESCE(?1, rate_limit_remaining),
                 rate_limit_limit = COALESCE(?2, rate_limit_limit),
                 rate_limit_reset = COALESCE(?3, rate_limit_reset)
                 WHERE user_id = ?4",
                params![rem, lim, reset, user_id],
            );
        }
    }
}
pub async fn fetch_and_sanitize_readme(
    state: &AppState,
    full_name: &str,
    default_branch: &str,
    pat: &str,
) -> Option<String> {
    let url = format!("https://api.github.com/repos/{}/readme", full_name);
    let mut req = state
        .http_github
        .get(&url)
        .header("Accept", "application/vnd.github.html+json");
    if !pat.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", pat));
    }

    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let raw_html = resp.text().await.ok()?;

    // Rewrite relative image src and link href attributes to raw.githubusercontent.com / github.com
    let raw_base = format!(
        "https://raw.githubusercontent.com/{}/{}/",
        full_name, default_branch
    );
    let blob_base = format!("https://github.com/{}/blob/{}/", full_name, default_branch);

    Some(rewrite_readme_urls(&raw_html, &raw_base, &blob_base))
}

/// Rewrite every relative `src` / `srcset` / `href` in GitHub-rendered README
/// HTML to an absolute URL.
///
/// GitHub returns paths without a leading `./` or `/` (`img/logo.svg`), which
/// the browser would otherwise resolve against this app's own origin and 404.
fn rewrite_readme_urls(html: &str, raw_base: &str, blob_base: &str) -> String {
    static ATTR_RE: OnceLock<Regex> = OnceLock::new();
    let re = ATTR_RE.get_or_init(|| {
        Regex::new(r#"(?i)\b(src|href|srcset)\s*=\s*(?:"([^"]*)"|'([^']*)')"#)
            .expect("README attribute regex is valid")
    });

    re.replace_all(html, |caps: &Captures<'_>| {
        let attr = caps
            .get(1)
            .map(|m| m.as_str().to_ascii_lowercase())
            .unwrap_or_default();
        let value = caps
            .get(2)
            .or_else(|| caps.get(3))
            .map(|m| m.as_str())
            .unwrap_or("");
        let base = if attr == "href" { blob_base } else { raw_base };
        let rewritten = if attr == "srcset" {
            rewrite_srcset(value, base)
        } else if is_absolute_url(value) {
            value.to_string()
        } else {
            join_relative(base, value)
        };
        format!("{attr}=\"{rewritten}\"")
    })
    .into_owned()
}

/// Absolute URLs, in-page anchors and non-navigable schemes are left alone.
fn is_absolute_url(value: &str) -> bool {
    let value = value.trim();
    if value.is_empty() || value.starts_with('#') || value.starts_with("//") {
        return true;
    }
    let lower = value.to_ascii_lowercase();
    ["http://", "https://", "data:", "mailto:", "tel:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
}

fn join_relative(base: &str, value: &str) -> String {
    let path = value
        .trim()
        .trim_start_matches("./")
        .trim_start_matches('/');
    format!("{base}{path}")
}

/// `img/a.png 1x, img/a@2x.png 2x` -> both candidates made absolute.
fn rewrite_srcset(value: &str, base: &str) -> String {
    value
        .split(',')
        .map(|candidate| {
            let candidate = candidate.trim();
            let mut parts = candidate.splitn(2, char::is_whitespace);
            let url = parts.next().unwrap_or("");
            let descriptor = parts.next().unwrap_or("").trim();
            if url.is_empty() || is_absolute_url(url) {
                return candidate.to_string();
            }
            if descriptor.is_empty() {
                join_relative(base, url)
            } else {
                format!("{} {}", join_relative(base, url), descriptor)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::{parse_github_slug, rewrite_readme_urls};

    #[test]
    fn accepts_slugs_and_urls() {
        assert_eq!(
            parse_github_slug("owner/repo").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            parse_github_slug("https://github.com/owner/repo/tree/main/src").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            parse_github_slug("  owner/repo.git/  ").as_deref(),
            Some("owner/repo")
        );
    }

    #[test]
    fn rejects_traversal_and_garbage() {
        assert_eq!(parse_github_slug("https://github.com/../rate_limit"), None);
        assert_eq!(parse_github_slug("owner/.."), None);
        assert_eq!(parse_github_slug("owner/..%2f.."), None);
        assert_eq!(parse_github_slug("owner/re po"), None);
        assert_eq!(parse_github_slug("not-a-repo"), None);
        assert_eq!(parse_github_slug("owner/"), None);
    }

    const RAW: &str = "https://raw.githubusercontent.com/owner/repo/main/";
    const BLOB: &str = "https://github.com/owner/repo/blob/main/";

    #[test]
    fn rewrites_bare_relative_images() {
        let html = r#"<img alt="logo" src="img/logo.svg">"#;
        assert_eq!(
            rewrite_readme_urls(html, RAW, BLOB),
            format!(r#"<img alt="logo" src="{RAW}img/logo.svg">"#)
        );
    }

    #[test]
    fn rewrites_dot_slash_and_root_relative_prefixes() {
        assert_eq!(
            rewrite_readme_urls(r#"<img src="./a.png">"#, RAW, BLOB),
            format!(r#"<img src="{RAW}a.png">"#)
        );
        assert_eq!(
            rewrite_readme_urls(r#"<img src="/a.png">"#, RAW, BLOB),
            format!(r#"<img src="{RAW}a.png">"#)
        );
    }

    #[test]
    fn rewrites_every_srcset_candidate_and_keeps_descriptors() {
        let html = r#"<img srcset="img/a.png 1x, img/a@2x.png 2x">"#;
        assert_eq!(
            rewrite_readme_urls(html, RAW, BLOB),
            format!(r#"<img srcset="{RAW}img/a.png 1x, {RAW}img/a@2x.png 2x">"#)
        );
    }

    #[test]
    fn rewrites_relative_links_to_blob_urls() {
        let html = r#"<a href="docs/setup.md">setup</a>"#;
        assert_eq!(
            rewrite_readme_urls(html, RAW, BLOB),
            format!(r#"<a href="{BLOB}docs/setup.md">setup</a>"#)
        );
    }

    #[test]
    fn leaves_absolute_anchors_and_data_urls_alone() {
        let html = concat!(
            r##"<a href="#usage">usage</a>"##,
            r#"<a href="https://example.com/x">x</a>"#,
            r#"<img src="data:image/svg+xml;base64,AAA">"#,
            r#"<img src="//cdn.example.com/x.png">"#
        );
        assert_eq!(rewrite_readme_urls(html, RAW, BLOB), html);
    }

    #[test]
    fn handles_single_quotes_and_uppercase_attributes() {
        assert_eq!(
            rewrite_readme_urls(r#"<IMG SRC='img/a.png'>"#, RAW, BLOB),
            format!(r#"<IMG src="{RAW}img/a.png">"#)
        );
    }

    #[test]
    fn keeps_attribute_prefixes() {
        // Lazy-loading and SVG-style attributes still have their prefix.
        assert_eq!(
            rewrite_readme_urls(r#"<img data-src="img/a.png">"#, RAW, BLOB),
            format!(r#"<img data-src="{RAW}img/a.png">"#)
        );
        assert_eq!(
            rewrite_readme_urls(r#"<use xlink:href="icons.svg#a">"#, RAW, BLOB),
            format!(r#"<use xlink:href="{BLOB}icons.svg#a">"#)
        );
    }
}
