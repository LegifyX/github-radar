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

---

## Building from Source

```bash
# Run locally in development mode
DATABASE_PATH=./data/radar.db cargo run

# Build optimized release binary
cargo build --release
```

---

## Project Structure

```text
github-radar/
├── .github/
│   └── workflows/
│       └── docker.yml      # Multi-stage Buildx CI/CD to ghcr.io
├── src/
│   └── main.rs             # Axum server, SQLite migrations, scheduler, GitHub & /v1 AI clients
├── static/
│   └── index.html          # Embedded single-file responsive web & mobile PWA frontend
├── Cargo.toml              # Rust dependencies and release LTO profile
├── Dockerfile              # Multi-stage build (rust:1-bookworm -> debian:bookworm-slim)
├── compose.yaml            # Docker Compose / Dockge stack definition
├── spec.md                 # Full architectural and product specification
└── README.md               # Project documentation
```