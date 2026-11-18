use axum::http::HeaderMap;
use chrono::DateTime;
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

    let rewritten = raw_html
        .replace("src=\"./", &format!("src=\"{}", raw_base))
        .replace("src=\"/", &format!("src=\"{}", raw_base))
        .replace("href=\"./", &format!("href=\"{}", blob_base));

    Some(rewritten)
}

#[cfg(test)]
mod tests {
    use super::parse_github_slug;

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
}
