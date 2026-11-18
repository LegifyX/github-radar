# GitHub Radar — Technical & Product Specification (`spec.md`)

**Version:** 1.0.0  
**Architecture:** Single-Binary Self-Hosted Rust Web Application & Progressive Web App (PWA)  
**Target Deployment:** Docker / Dockge on TrueNAS Scale with Persistent `/data` SQLite Storage  

---

## 1. Executive Summary & Core Architecture

**GitHub Radar** is a multi-user, self-hosted GitHub repository discovery and multi-branch commit monitoring Progressive Web App (PWA). It compiles into a single portable Linux binary (`github-radar`) with all frontend assets (`index.html`, `manifest.json`, `sw.js`, and SVG icons) embedded at compile time via `include_str!`.

### 1.1 Technology Stack
* **Backend:** Rust (`axum 0.7`, `tokio 1.40`, `reqwest 0.12` with `rustls-tls`, `serde`, `serde_json`, `chrono`, `bcrypt`, `uuid`).
* **Database:** SQLite via `rusqlite 0.32` (`bundled` amalgamation) running in **WAL (`Write-Ahead Logging`)** mode with `busy_timeout = 5000ms` and `synchronous = NORMAL`. Stored at `/data/radar.db` (configurable via `DATABASE_PATH`).
* **Frontend:** Vanilla HTML5/CSS3/ES6 JavaScript embedded inside `static/index.html` with zero external CDN dependencies, zero runtime build tools, and full PWA support (`/manifest.json`, `/sw.js`, `/icon.svg`).
* **Containerization & CI/CD:** Multi-stage `Dockerfile` (`rust:1-bookworm` builder → `debian:bookworm-slim` runtime with `ca-certificates` and `curl`), `compose.yaml` for Dockge, and `.github/workflows/docker.yml` publishing `linux/amd64` images to `ghcr.io`. The build copies `Cargo.lock` and compiles with `--locked`, so the image contains exactly the dependency set that was tested.

---

### 1.2 Source Layout

`main.rs` is wiring only (config, state, scheduler spawn, HTTP serve, graceful shutdown). Everything else has one home:

| Module | Responsibility |
| :--- | :--- |
| `state.rs` / `error.rs` / `models.rs` | `AppState`, `AppError` (JSON + status mapping), SQLite row models |
| `auth.rs` + `api/auth.rs` | Session-cookie extractor, setup/login/logout endpoints |
| `api/` | One module per route group (`assets`, `state`, `queries`, `repos`, `settings`, `backup`, `admin`); `api/mod.rs` holds the complete route table |
| `db/mod.rs` | Schema creation, boot-recovery sweep, per-user settings access |
| `scanner/` | Scan trigger/lock and daily maintenance (`mod`), discovery sweep (`discoveries`), multi-branch tracked sync (`tracked`) |
| `integrations/` | GitHub I/O and rate-limit capture (`github`), OpenAI-compatible `/v1` client (`ai`), Discord/Slack/generic webhooks (`webhook`) |

The four frontend assets live as real files under `static/` and are embedded with `include_str!`.

---

## 2. Design Language & Responsive UI/UX Architecture

### 2.1 Visual Identity ("GitHub Primer Dark + Linear")
* **Color Tokens:**
  * Canvas Background: `#0d1117`
  * Elevated Surfaces (Cards, Sidebars, Header): `#161b22` with `1px solid #30363d` hairline borders
  * Interactive Hover / Active Selection: `#1f242c` with `#58a6ff` left-border indicator
  * Primary / Links / Branches: `#58a6ff` (GitHub Blue)
  * Positive / Track / Force Refresh: `#238636` (GitHub Emerald)
  * AI Summary Callouts: Indigo-tinted surface (`#1e1b4b` background, `#3730a3` border, `#c7d2fe` text) to visually separate LLM insights from raw GitHub metadata
  * Warnings / Renamed (`301`): `#d29922` (Amber)
  * Danger / Missing (`404`) / Hard Delete: `#f85149` (Crimson)
* **Typography:** System UI sans-serif stack for interface text; monospace (`JetBrains Mono`, `Fira Code`, `ui-monospace`) at `12.5px` for search queries, commit SHAs, branch pills, and topic tags.

### 2.2 Adaptive Desktop (`>= 768px`) vs. Mobile PWA (`< 768px`) Layout
* **Top Header Bar:**
  * Left: Radar SVG logo + `GitHub Radar` title.
  * Center (Desktop): Horizontal tab bar (`Discoveries`, `Tracked`, `Ignored`, `Configuration`) with live count badges.
  * Right: Live 1-second countdown pill (`Next refresh in MMm SSs` or pulsing `Scanning...` indicator) synced to server-side `next_scan_at`, plus a **Force Refresh** button. Condensed on mobile to prevent text clipping.
  * Missing PAT Banner: Dismissible/clickable amber warning bar shown whenever `github_pat` is empty, alerting the user to unauthenticated rate limits (`10 req/min`).
* **Mobile PWA Ergonomics:**
  * Uses `100dvh` (`dynamic viewport height`) to prevent virtual keyboard/address-bar clipping.
  * Enforces `16px` input font size on `< 768px` to prevent iOS Safari auto-zoom on focus.
  * Sticky bottom thumb-navigation bar with `padding-bottom: env(safe-area-inset-bottom)` for iOS/Android standalone home-screen mode.
  * Service Worker (`/sw.js`) uses **Network-First** for `/` (`index.html`) while storing each successful response, so the app shell still opens offline; **Cache-First** for `/manifest.json` and `/icon.svg`; and **strictly bypasses** `/api/*` endpoints so every data view is live.

---

## 3. Authentication, Multi-User Isolation & Security

* **First-User Admin Bootstrap:** When `SELECT COUNT(*) FROM users` equals `0`, `/` presents a **Create Admin Account** form (`POST /api/auth/setup`). The first user automatically receives `is_admin = 1`. Once created, public registration is permanently disabled.
* **Password Hashing:** `bcrypt::DEFAULT_COST`.
* **30-Day Sliding Sessions:** Stored in SQLite `sessions (token, user_id, expires_at)` and issued as `HttpOnly; SameSite=Lax; Path=/; Max-Age=2592000` cookies (`radar_session`). Every authenticated request extends `expires_at` by 30 days and joins `users.is_active = 1`.
* **Immediate Session Revocation:** Disabling or deleting a user in the Admin Panel immediately deletes their rows from `sessions`, revoking access across all desktop and mobile clients. Resetting a user's password does the same, so a previously issued cookie cannot outlive the reset.
* **Write-Only Secrets:** `github_pat` and `ai_api_key` are stored in SQLite but never serialized back to the client. `/api/state` exposes only `github_pat_set` / `github_pat_hint` (masked, last 4 characters) and the `ai_api_key_*` equivalents. An empty secret field on save means "keep the stored value"; removal requires the explicit `clear_github_pat` / `clear_ai_api_key` flags. The GitHub and AI test buttons, plus every scanner path, fall back to the stored secret when none is supplied.
* **Cookie Hardening:** Session cookies are `HttpOnly; SameSite=Lax`. `Secure` is added when `COOKIE_SECURE=1`, which operators set when TLS terminates in a reverse proxy; the default keeps plain-HTTP LAN installs usable.
* **Per-User Data Isolation:** Every table (`user_settings`, `search_queries`, `repos`, `commits`, `deleted_tombstones`) is scoped by `user_id` with `ON DELETE CASCADE`.

---

## 4. Tab-by-Tab Functional Specification

### 4.1 Tab 1: Discoveries (Email-Style 2-Column Triage)
* **Left Column — Search Rules Manager (Collapsible Drawer on Mobile):**
  * Full CRUD (`POST /api/queries`, `PUT /api/queries/:id`, `DELETE /api/queries/:id`) + enable/disable toggle per rule.
  * Fields per rule:
    * `query_text`: Standard GitHub repository search syntax (e.g., `topic:ai-agent language:rust`).
    * `min_stars`: Numeric star floor (default `0`). Appends `stars:>=N` if `> 0` and not already present in `query_text`.
    * `initial_lookback_days`: Numeric look-back window (default `7`). Applied as `pushed:>YYYY-MM-DD` **only** on the rule's first run (`last_run_at IS NULL`). Subsequent scans use `pushed:>last_run_at - 10m` overlap buffer, deduplicated by `(user_id, github_repo_id)`.
    * `notify_webhook`: Boolean toggle determining whether newly discovered repos from this rule trigger the Discovery Webhook stream.
* **Right Column — Discovery Feed:**
  * Displays repositories where `status = 'discovery' AND ignore_type = 'none' AND discovered_at > NOW() - auto_ignore_days`, sorted by `pushed_at DESC`.
  * Live client-side filter bar searching repo name, description, language, topic tags, matched query, and AI summary.
  * **Card Anatomy:** `owner/repo` link, external `↗ GitHub` button, star count, primary language, relative `pushed_at` timestamp, matched search query badge, **all GitHub topic tags (`topics`)** rendered as pills, GitHub description, optional **AI README Summary** box (`pending` / `completed` / `failed` + retry button), and **Track** / **Ignore** buttons.
  * **Optimistic UI & Race-Safe Bulk Ignore:** Clicking **Track** or **Ignore** animates the card out (`140ms`) immediately. **"Ignore All Visible"** posts the explicit array of currently rendered `repo_ids` (`POST /api/repos/ignore-batch`) so newly scanned background items arriving mid-click are never accidentally ignored.

### 4.2 Tab 2: Tracked (Master-Detail Split & Mobile Slide-Over)
* **Left Column — Watchlist Sidebar:**
  * **Quick-Track Input:** Parses any `owner/repo` slug or full GitHub URL (`https://github.com/owner/repo/tree/main/...`, `.git`, etc.), fetches metadata via GitHub API, upserts into `status = 'tracked'`, and triggers an immediate baseline fetch of commits and `README.md`.
  * **Toolbar:** Live text filter (searches name, description, topics, AI summaries), **Unread Only** checkbox, and Sort selector (**Custom Order**, **Recent Activity**, **Stars**).
  * **Watchlist Item Controls:**
    * Pin toggle (`📌`) pinning items to the top (`is_pinned DESC, sort_order ASC`).
    * Up/Down (`↑` / `↓`) custom reorder buttons (automatically disabled when filtered or sorted by non-custom order to prevent index confusion).
    * Per-repo webhook toggle (`🔔` / `🔕`) controlling Tracked Repo Update notifications.
    * **Status Badges:** Unread commit counter (`N new`), **Renamed (`301`)** badge (`Renamed from old/name`, dismissible), and **404 Unavailable** warning badge.
* **Right Column — Split Detail View (Full-Screen Slide-Over with `← Back` & `Commits | README` Toggle on Mobile):**
  * **Half 1 — Multi-Branch Commit Timeline:**
    * Scans up to the top 5 active branches (always prioritizing `default_branch`), deduplicates commits by `(user_id, repo_id, sha)`, and aggregates branch names into a JSON array (`branches_json`) so merged commits display all branch pills (`main`, `feat/...`) without duplication.
    * Displays optional **AI Commit Diff Summary** (with lockfiles like `Cargo.lock`, `package-lock.json`, `yarn.lock`, `.min.js`, and `.svg` stripped, and patches truncated to stay within small local LLM context limits).
    * **Mark Read** button clears `unread_count` and sets `is_read = 1`.
  * **Half 2 — Cached Repository `README.md` Viewer:**
    * Fetches GitHub-rendered, sanitized HTML (`Accept: application/vnd.github.html+json`), rewrites relative `./` and `/` image `src` URLs to `https://raw.githubusercontent.com/{owner}/{repo}/{default_branch}/` and relative `href` links to `https://github.com/{owner}/{repo}/blob/{default_branch}/`, and caches `readme_html` in SQLite for instant (`0ms`) offline viewing. Includes a **↻ Refresh README** button.

### 4.3 Tab 3: Ignored (Manual & Retroactive Auto-Expired)
* **Non-Destructive Retroactive Windowing:**
  * No destructive background cron job deletes or mutates expired discoveries. Instead, `discovered_at` and `ignore_type` (`'none' | 'manual'`) are evaluated dynamically against `user_settings.auto_ignore_days`.
  * Increasing `auto_ignore_days` in Configuration (e.g., from `3` to `7` days) immediately restores 4-to-6-day-old auto-expired discoveries back into Tab 1 while keeping `ignore_type = 'manual'` repos ignored.
* **Actions & Tombstone Anti-Resurrection:**
  * Distinguishes `Manually Ignored` vs. `Auto-Expired` via badges.
  * **Track Anyway:** Promotes the repo to Tab 2 (`status = 'tracked'`).
  * **Delete Record (`DELETE /api/repos/:id`):** Hard-deletes the repo from `repos` and inserts `(user_id, github_repo_id, deleted_at)` into `deleted_tombstones`. The scanner ignores the repo unless `repo.pushed_at > deleted_at` in the future.

### 4.4 Tab 4: Configuration & Admin Panel
1. **Session & Account Status:** Displays logged-in username, admin badge, 30-day session expiration date, and **Sign Out** button.
2. **GitHub PAT & Rate Limit Inspector:** Write-only PAT input (placeholder shows `Saved (…last4)` when a token is stored), **Test Token & Rate Limit** button (queries `/rate_limit` and displays Core + Search quotas; a blank field tests the stored token), **Clear** action, and expandable step-by-step setup guide for fine-grained read-only public tokens.
3. **Scanner & Retroactive Timer Settings:** `refresh_frequency_minutes` (5–1440), `auto_ignore_days` (1–365), and `max_results_per_query` (1–100, default `15`).
4. **Customizable Dual Webhook Streams:**
   * **Stream 1 (Search Discoveries):** URL + template supporting `{{query}}`, `{{repo}}`, `{{url}}`, `{{stars}}`, `{{description}}`.
   * **Stream 2 (Tracked Repo Updates):** URL + template supporting `{{repo}}`, `{{branch}}`, `{{commits}}`, `{{author}}`, `{{date}}`, `{{url}}`, `{{ai_summary}}`.
   * Includes **"Use Discovery Webhook for Both"** button, **Test** buttons, and `serde_json`-escaped payloads (`{"content": ...}` for Discord, `{"text": ...}` for Slack, and dual keys for generic endpoints).
5. **Optional OpenAI-Compatible `/v1` AI Provider:** Master toggle, `base_url` (e.g., `http://ollama:11434/v1`), optional write-only `api_key` (same masked/keep/clear semantics as the PAT), `model` (e.g., `gemma2:2b`), **Test AI Connection** button, and independent checkboxes for **Summarize new repo READMEs** and **Summarize tracked commit diffs**.
6. **JSON Backup Export / Import:** Exports/imports settings, deduplicated search queries, and tracked/manually-ignored repositories (preserving pins, sort order, and webhook toggles).
7. **Admin Control Panel (Admins Only):** Create new user accounts, reset passwords, enable/disable accounts (with immediate session revocation), and delete non-admin users.

---

## 5. 24/7 Server Resilience & Operational Guarantees

* **Boot Recovery Sweep:** On process startup, `init_db` resets `UPDATE user_settings SET is_scanning = 0` and `UPDATE repos SET ai_status = 'none' WHERE ai_status = 'pending'`, preventing permanent deadlocks after a TrueNAS reboot or `SIGKILL`.
* **API Error Contract:** Client errors return `{"error": "…"}` with meaningful status codes — `404` for missing records, `409` for unique-constraint conflicts (e.g. duplicate username), `401`/`403` for session/permission failures. SQLite internals are logged server-side, never returned to the browser.
* **Per-User Deterministic Scheduler:** A 10-second Tokio ticker checks `WHERE is_active = 1 AND is_scanning = 0 AND next_scan_at <= NOW()`, backed by an in-memory `Mutex<HashSet<i64>>` scan guard.
* **Transactional Cursor Advancement:** `search_queries.last_run_at` is updated **only** after a successful HTTP `200` response from GitHub. Multi-day internet or GitHub outages never skip missed repositories once connectivity returns.
* **Decoupled Bounded AI Queue:** LLM requests run asynchronously behind a `Semaphore::new(1)` with a 10s connect / 90s read timeout so slow local GPUs never block GitHub scanning or thrash VRAM.
* **Daily SQLite Maintenance:** Runs every 24 hours to prune expired `sessions`, prune `deleted_tombstones` older than 90 days, cap `commits` to the latest 100 per tracked repo, and execute `PRAGMA wal_checkpoint(TRUNCATE); PRAGMA optimize;`.
* **Graceful Shutdown:** Intercepts `SIGTERM` / `SIGINT` from Docker/Dockge, clears scan flags, and checkpoints the SQLite WAL file before exit.

---

## 6. Database Schema (`SQLite`)

* `users (id, username, password_hash, is_admin, is_active, created_at)`
* `sessions (token, user_id, expires_at)`
* `user_settings (user_id, github_pat, refresh_frequency_minutes, auto_ignore_days, max_results_per_query, discovery_webhook_url, discovery_webhook_template, tracked_webhook_url, tracked_webhook_template, ai_enabled, ai_base_url, ai_api_key, ai_model, ai_summarize_readme, ai_summarize_commits, last_scan_at, next_scan_at, is_scanning, last_error, rate_limit_remaining, rate_limit_limit, rate_limit_reset)`
* `search_queries (id, user_id, query_text, min_stars, initial_lookback_days, notify_webhook, is_enabled, last_run_at, created_at)`
* `repos (id, user_id, github_repo_id, full_name, html_url, description, language, stars, topics_json, pushed_at, default_branch, status, ignore_type, matched_query, discovered_at, is_pinned, sort_order, notify_webhook, unread_count, was_renamed, previous_name, is_missing, readme_summary, diff_summary, ai_status, readme_html, readme_updated_at)`
* `commits (id, user_id, repo_id, sha, branches_json, author, message, committed_at, html_url, is_read)`
* `deleted_tombstones (user_id, github_repo_id, deleted_at)`