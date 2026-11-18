# GitHub Radar

A self-hosted, single-binary Rust web application and Progressive Web App (PWA) for discovering new GitHub repositories via automated search rules and monitoring multi-branch commit activity on tracked repositories—complete with optional OpenAI-compatible (`/v1`) AI summaries, Discord/Slack/generic webhooks, and multi-user authentication.

Designed specifically to deploy cleanly via **Docker / Dockge on TrueNAS** with a single persistent SQLite database (`/data/radar.db`).

---

## Key Features

* **Single-Binary Portability:** All frontend HTML, CSS, JavaScript, PWA `manifest.json`, Service Worker (`sw.js`), and SVG icons are embedded directly into the compiled Rust binary—zero external CDN dependencies or asset path issues.
* **4-Tab Responsive & Mobile PWA UI:**
  * **Tab 1 — Discoveries:** Manage modular GitHub search queries (`query_text`, per-query `min_stars` floor, `initial_lookback_days`, and per-rule webhook toggle). Triage newly discovered repositories with optimistic UI cards, topic pills, AI `README.md` summaries, and race-safe **"Ignore All Visible"** batching.
  * **Tab 2 — Tracked:** Watchlist sidebar with **Quick-Track** (paste any `owner/repo` or GitHub URL), live search, **Unread Only** filter, custom ordering (`📌` Pin + `↑`/`↓`), per-repo webhook toggles (`🔔`), automatic **Rename (`301`)** & **Unavailable (`404`)** detection, merged multi-branch commit timeline, AI commit diff summaries (with lockfiles stripped), and an instant cached `README.md` viewer.
  * **Tab 3 — Ignored:** Non-destructive retroactive auto-expiration window. Adjusting the auto-ignore days in Configuration immediately restores or hides auto-expired discoveries without affecting manually ignored items. Includes hard-delete with tombstone tracking so deleted repos only return if pushed in the future.
  * **Tab 4 — Configuration & Admin Panel:** GitHub PAT tester & rate-limit inspector, scanner intervals, dual customizable webhook streams (Discord/Slack/generic), OpenAI-compatible `/v1` AI endpoint configuration (Ollama, vLLM, OpenAI), JSON backup export/import, and an Admin User Management panel.
* **24/7 Server Resilience:** SQLite WAL mode with automatic daily maintenance/checkpointing, boot-time stale lock recovery, transactional scan cursors that survive internet outages without skipping repos, bounded AI queues, and graceful `SIGTERM` shutdown.

---

## Quick Start (Docker / Dockge on TrueNAS)

### 1. Create a Persistent Dataset
Create a dataset or directory on your host for the SQLite database (e.g., `/mnt/tank/docker/github-radar`).

The container runs as **UID/GID `10001`** (non-root), so that directory has to be writable by it. On TrueNAS:

```bash
# tidy: give the data folder to the container user
sudo chown -R 10001:10001 /mnt/tank/docker/github-radar

# or, if you'd rather not track a numeric UID on the share:
sudo chmod -R 777 /mnt/tank/docker/github-radar
```

A Docker **named volume** (`github-radar_data:/data`) inherits the right ownership from the image and needs no action. If the folder stays owned by someone else, the container will fail to create `/data/radar.db` and exit; alternatively pin a different UID in the compose file (`user: "1000:1000"`) to match the host owner.

### 2. Deploy via `compose.yaml`
Create a stack in **Dockge** (or run `docker compose up -d`) using:

```yaml
services:
  github-radar:
    image: ghcr.io/<your-github-username>/github-radar:latest
    container_name: github-radar
    restart: unless-stopped
    ports:
      - "8080:8080"
    environment:
      - DATABASE_PATH=/data/radar.db
      - BIND_ADDR=0.0.0.0:8080
      - RUST_LOG=github_radar=info
    # Container runs as UID/GID 10001; see the permissions note above.
    # user: "1000:1000"   # optional: run as your own NAS user instead
    volumes:
      - /mnt/tank/docker/github-radar:/data
    healthcheck:
      test: ["CMD", "curl", "-f", "http://localhost:8080/api/health"]
      interval: 30s
      timeout: 5s
      retries: 3
      start_period: 10s
```

*(To build locally from source instead of pulling from `ghcr.io`, replace `image:` with `build: .`.)*

### 3. Initial Admin Setup & Configuration
1. Open `http://<server-ip>:8080` in your browser.
2. When no users exist yet, the **Create Admin Account** bootstrap form will appear. Create your primary administrator account (sessions persist for 30 days via `HttpOnly` cookies).
3. Navigate to **Configuration** to add a read-only **GitHub Personal Access Token (PAT)** (raises API limits from 60/hr to 5,000/hr) and optionally configure webhooks or your `/v1` AI endpoint.
4. *(Optional)* Place GitHub Radar behind an HTTPS reverse proxy (Caddy, Nginx Proxy Manager, or Traefik) to install it as a standalone PWA on iOS and Android home screens.

---

## Environment Variables

| Variable | Default | Description |
| :--- | :--- | :--- |
| `DATABASE_PATH` | `/data/radar.db` | File path for the SQLite database (parent directories are created automatically). |
| `BIND_ADDR` | `0.0.0.0:8080` | TCP host and port the Axum HTTP server binds to. |
| `RUST_LOG` | `github_radar=info` | Tracing log verbosity (`error`, `warn`, `info`, `debug`). |
| `COOKIE_SECURE` | `0` | Set to `1` when TLS terminates in front of the container, so the session cookie is marked `Secure`. Leave `0` for plain-HTTP LAN installs. |

---

## Security Notes

* **Tokens never reach the browser.** The GitHub PAT and the optional AI API key are stored in SQLite and are never serialized into API responses. `/api/state` only returns `github_pat_set` / `github_pat_hint` (e.g. `…a1b2`), and the Configuration form treats an empty field as "keep the stored value". Clearing requires the explicit **Clear** button.
* **Sessions** are 30-day sliding, `HttpOnly; SameSite=Lax` cookies. Set `COOKIE_SECURE=1` once the app is behind HTTPS.
* **Password resets revoke sessions.** Resetting a user's password (or disabling/deleting the account) deletes their session rows immediately.
* **Sign-in throttling.** Five failed attempts for the same username in 15 minutes lock further attempts for that window (HTTP 429), so the bcrypt cost cannot be used as a free CPU burn.
* **Browser hardening.** Responses carry a Content-Security-Policy (same-origin scripts, styles and XHR; remote images only), `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY` and `Referrer-Policy: no-referrer`.

---

## Building from Source

```bash
# Run locally in development mode
DATABASE_PATH=./data/radar.db cargo run

# Build optimized release binary
cargo build --release
```

> **Windows note:** MSVC's `-Brepro` flag breaks when the object-file path contains spaces (`C:\...\AI Playground\...`), producing `cl : Command line error D8050`. If your checkout path has a space, point the target directory somewhere space-free:
>
> ```powershell
> $env:CARGO_TARGET_DIR = "C:\cargo-targets\github-radar"; cargo build
> ```

---

## Project Structure

```text
github-radar/
├── .github/workflows/
│   ├── ci.yml                    # fmt + clippy -D warnings + tests
│   └── docker.yml                # Buildx CI/CD → ghcr.io
├── src/
│   ├── main.rs                   # Env config, AppState wiring, scheduler spawn, serve, shutdown
│   ├── state.rs                  # AppState (DB handle, HTTP clients, scan guard, cookie policy)
│   ├── error.rs                  # AppError → JSON error responses, SQLite error mapping
│   ├── models.rs                 # UserSettings / SearchQuery / RepoRecord / CommitRecord
│   ├── auth.rs                   # AuthUser extractor (session cookie → user)
│   ├── api/
│   │   ├── mod.rs                # The complete route table
│   │   ├── assets.rs             # Embedded index.html / manifest / service worker / icon
│   │   ├── auth.rs               # Setup, login, logout, status
│   │   ├── state.rs              # GET /api/state, POST /api/scan/force
│   │   ├── queries.rs            # Search-rule CRUD
│   │   ├── repos.rs              # Quick-track, triage, reorder, details, README, AI, delete
│   │   ├── settings.rs           # Settings + GitHub/webhook/AI test endpoints
│   │   ├── backup.rs             # JSON export / import
│   │   └── admin.rs              # User management
│   ├── db/mod.rs                 # Schema creation, boot recovery sweep, settings access
│   ├── scanner/
│   │   ├── mod.rs                # Scan trigger/lock, daily maintenance
│   │   ├── discoveries.rs        # Search-rule execution, dedupe, discovery webhooks
│   │   └── tracked.rs            # Multi-branch commit sync, rename/404 detection, tracked webhooks
│   └── integrations/
│       ├── github.rs             # Slug parsing, rate-limit capture, README sanitizing/rewrite
│       ├── ai.rs                 # OpenAI-compatible /v1 client + README/diff summaries
│       └── webhook.rs            # Discord / Slack / generic payload rendering
├── static/
│   ├── index.html                # Single-file responsive web & mobile PWA frontend
│   ├── manifest.json             # PWA manifest (embedded at compile time)
│   ├── sw.js                     # Service worker (embedded at compile time)
│   └── icon.svg                  # App icon (embedded at compile time)
├── Cargo.toml / Cargo.lock       # Pinned dependencies, MSRV and release LTO profile
├── rust-toolchain.toml           # stable + rustfmt/clippy for contributors
├── .dockerignore                 # Keeps target/, data/ and .git out of the build context
├── Dockerfile                    # Multi-stage build (rust:1-bookworm → debian:bookworm-slim)
├── compose.yaml                  # Docker Compose / Dockge stack definition
├── SPEC.md                       # Full architectural and product specification
├── LICENSE                       # MIT
└── README.md                     # Project documentation
```

---

## License

MIT — see [LICENSE](LICENSE).
