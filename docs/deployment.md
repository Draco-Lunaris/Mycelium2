# Deployment

## Docker (recommended)

```sh
docker compose up -d
```

The compose file publishes 443 (HTTPS) and 80 (HTTP redirect) and
mounts the `mycelium2-data` volume at `/opt/mycelium2/data`. All state
lives there: SQLite database, encrypted files, config, TLS certs
(auto-generated), assets.

First boot:

1. The server creates the data layout and runs migrations. No admin
   account and no bootstrap files are created.
2. Two packaged skills — `pdf-to-markdown` (PDF book → enhanced
   markdown ready for library ingest) and `ebook-to-markdown`
   (epub/mobi/azw3/fb2 → the same kind of enhanced markdown book,
   via the calibre engine) — are seeded into the global skills shelf
   automatically. Every user can read them from `/skills`; admins can
   edit. Upgrades refresh them when the packaged version changes; admin
   edits are preserved between upgrades. A shelf seeded by an earlier
   release sits at seed v1 (flat `pdf-to-markdown`) or v2 (nested
   `pdf-to-markdown` only), depending on when it last upgraded; one
   boot of this release brings either state to seed v3 — from v1, the
   legacy flat paths are deleted and both skills' nested layouts are
   written; from v2, the pdf layout is already correct and the ebook
   skill is added nested — this boot is a version-bump refresh, so
   admin edits to the pdf packaged paths are overwritten, as with
   any bump. Admin edits to the legacy flat paths do not survive the
   v1 migration — copy them out first. Admin edits to the packaged
   paths survive between upgrades; the refresh that bumps the packaged
   version — or a lost `.seed-version` marker, which triggers the same
   refresh — overwrites them.
3. Open `https://<host>/setup` and create the admin account (username
   and password of your choice; recovery key shown once — record it).
4. Log in at `https://<host>/`. `/setup` becomes unavailable once the
   first account exists.

### TLS

By default the server generates a self-signed certificate on first
boot and stores it in the data directory. To use real certificates,
mount them and set the env vars:

```yaml
    environment:
      MYCELIUM2_TLS_CERT: /opt/mycelium2/tls/fullchain.pem
      MYCELIUM2_TLS_KEY: /opt/mycelium2/tls/privkey.pem
    volumes:
      - ./certs/fullchain.pem:/opt/mycelium2/tls/fullchain.pem:ro
      - ./certs/privkey.pem:/opt/mycelium2/tls/privkey.pem:ro
```

### Behind a reverse proxy

The binary terminates TLS itself. If you prefer an upstream proxy
(Let's Encrypt automation, etc.), point the proxy at the HTTPS port
with `proxy_pass https://127.0.0.1:8443` and set
`MYCELIUM2_HTTPS_ADDR=127.0.0.1:8443`. Keep the proxy's
`X-Forwarded-*` handling consistent; sessions are cookie-based and
host-agnostic.

## Binary

```sh
mycelium2 \
  --data-dir /opt/mycelium2/data \
  --https-addr 0.0.0.0:443 \
  --http-addr 0.0.0.0:80
```

All flags have `MYCELIUM2_*` env equivalents (see
[Configuration](configuration.md)).

## Systemd (binary deployment)

```ini
[Unit]
Description=Mycelium2
After=network.target

[Service]
User=mycelium
Group=mycelium
Environment=MYCELIUM2_DATA_DIR=/opt/mycelium2/data
ExecStart=/usr/local/bin/mycelium2
Restart=on-failure
# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/opt/mycelium2/data
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

## Health and monitoring

- `GET /health` — public JSON status (unauthenticated by design, for
  container orchestration). It also reports the mutation queue —
  `queue_depth`, `oldest_pending_age_seconds`, `queue_dead_count`,
  `llm_last_success_seconds` — and answers 503 `degraded` when the
  database is unreachable, any queued item is dead, or a pending item
  has waited past twice the integration deadline. Alongside those it
  reports the LLM admission gate — `llm_active` (runs holding a slot),
  `llm_queued` (waiters) and `llm_capacity` (configured slots). Those
  three are
  observability only — a saturated or queued gate is load, not ill
  health, and never makes the status `degraded`.
- `GET /api/v1/health` — detailed JSON status (authenticated).
- `GET /metrics` — Prometheus counters (logins, searches, ingests,
  backups, mutation-queue totals, ...) plus gauges for the mutation
  queue depth, the oldest pending item's age, and
  `mycelium2_llm_last_success_timestamp` (last successful LLM
  completion; absent until the first success) — public by design for
  scrape endpoints; put it behind a firewall or reverse-proxy auth if
  your threat model requires.
- LLM-gate metric lines (all seven from one consistent read per
  scrape):
  - `mycelium2_llm_gate_active` (gauge) — runs holding a slot
  - `mycelium2_llm_gate_queued` (gauge) — waiters in the FIFO queue
  - `mycelium2_llm_gate_capacity` (gauge) — configured slot count
  - `mycelium2_llm_gate_admissions_total` (counter) — slots granted
    since start, both admission classes
  - `mycelium2_llm_gate_timeouts_total` (counter) — wait-class
    waiters that gave up, including the ones released at shutdown
  - `mycelium2_llm_gate_wait_seconds_total` (counter) — cumulative
    queue wait of admitted wait-class runs
  - `mycelium2_llm_gate_wait_count` (counter) — samples behind that
    total; average wait = `wait_seconds_total / wait_count`
- Backend-health metric lines (all from one health-registry read per
  scrape): `mycelium2_backend_up` (gauge; `0`=disabled — no stored LLM
  config row, `1`=red, `2`=yellow, `3`=green),
  `mycelium2_backend_down_seconds` (gauge; seconds the outage has been
  open, `0` when not down), and `mycelium2_chat_waiting_turns` (gauge;
  chat turns owed an answer). The `/health` JSON carries the same
  facts as `backend_state`, `backend_down_seconds`,
  `backend_checked_seconds_ago`, and `chat_waiting_count`. An
  unhealthy backend never makes `/health` `degraded` — the LLM backend
  being down is weather these services wait out, not illness of this
  container; degraded still means MY database or write queue is ill.
- Container healthcheck: the HTTP redirect listener answers on port
  80 (`curl http://127.0.0.1:80/`).

### Backend outages (what operators see, what recovers on its own)

A background probe asks the configured LLM backend `GET {url}/models`
every 30 s (5 s timeout). One or two consecutive failures report
**yellow**; three or more report **red**; no stored LLM config row is
**disabled** (grey — nothing to probe, not an outage). The admin
**System page** (`/admin/system`) shows the state as a dot + word (and
self-refreshes every 30 s), a waiting-summary line, and a table of
every non-terminal waiting item (parked chat turns, queued writes,
pending ingest jobs — metadata only, never transcript content).

While the backend is red:

- New chat messages are accepted and parked `waiting_backend`; parked
  and crash-interrupted turns are **retried periodically** (the 60 s
  resume tick) and complete automatically when the backend returns —
  no restart, no manual nudge, and no permanent failed verdict from
  the downtime itself.
- The mutation-queue drain keeps draining through the outage — it has
  no health gate. Agent integration attempts fail fast against the
  dead backend, the per-item retry backoff (30/60/120 s) runs, and the
  item then integrates through the deterministic fallback: queued
  writes make progress without the LLM, exactly as the write-queue
  design intended. This is safe to run during an outage: the drain
  admits with `try_enter` only, so it never takes gate capacity from
  interactive work or queues ahead of it — and while the backend is
  down, interactive work isn't waiting on it either (chat turns are
  parked, not queued). Only the *staging* rows wait (below), and only
  for a key, never for the backend.
- Staged write payloads whose **user key** is temporarily unavailable
  wait **in place**: the row stays `staging` (no dead verdict, no
  retry timer) and activates when the key becomes recoverable again.
  (The original spec §3.4 wording said such rows are deferred back to
  `pending`; the implementation skips in place instead — the outcome
  operators care about is identical: the item waits for the key, it
  never dies from key loss.)
- Ingest jobs stay pending and run on a later worker tick.

A reachable-but-flapping backend (yellow) still runs work: a flap is
not an outage, and failing fast on yellow would throw away recovery
opportunities. A corrupt or foreign-sealed `llm` config row is a
config problem, not backend weather: the probe logs a warning each
tick and reports **no** observation (the last real state stays), so a
bad row never drives the dot red.

### Sizing `max_parallel`

`max_parallel` (Admin → LLM, default 2) is the number of agent runs
allowed to talk to the backend at once — size it to what the backend
actually serves in parallel, not to the number of users. A local
Ollama or a single-GPU gateway usually runs one or two generations
before latency collapses; a hosted multi-replica endpoint can take
more. Raise it only while watching `mycelium2_llm_gate_active`
against the backend's own throughput: beyond the backend's real
parallelism, queueing at the gate is cheaper than queueing inside the
model server, because waiters keep their FIFO place, report a
position, and give up at a bound instead of timing out mid-request.
If `mycelium2_llm_gate_queued` is persistently high while the backend
is not saturated, the gate — not the model — is the limit; if
`mycelium2_llm_gate_timeouts_total` is climbing, waiters are giving up
and either the backend or the bounds need attention. Changes apply
immediately, so tuning needs no restart.

## Upgrading

The image is data-free: pull the new image and restart. Migrations run
automatically on startup. Take a backup first (see
[Backup and restore](backup.md)).

### Packaged skills (seed v3)

The packaged `pdf-to-markdown` and `ebook-to-markdown` skills seed
as nested bundles (`SKILLS_SEED_VERSION` 3):

1. Fresh boots write the nested layouts directly: each skill's
   `/<slug>/skill.md` hub (carrying the bundle manifest), its
   companion concepts under `/<slug>/`, and the scripts as raw
   payload files — scripts are never concepts (no registry row, no
   search entry).
2. A shelf seeded by an older release migrates on the first boot of
   the new binary, from either earlier seed: from seed v1 (the flat
   pdf layout), the legacy flat `/pdf-to-markdown*.md` concepts are
   deleted (idempotently) and both nested bundles are written; from
   seed v2 (nested pdf only), the pdf layout is already correct and
   the ebook bundle is added nested — this boot is a version-bump
   refresh, so admin edits to the pdf packaged paths are overwritten,
   as with any bump. The flat-deletion concern is pdf-only — the
   ebook skill never shipped flat, so it installs nested either way.
   Admin edits to the legacy flat packaged paths do not survive the
   v1 migration — copy them out first if you modified them.
3. The `.seed-version` marker gates refresh as before: a matching
   marker is a no-op (admin edits to the packaged paths survive
   between bumps), and extra admin-created skills are never touched.
   Seeding validates the embedded concepts and manifest paths before
   any store write and fails fast on error — the server refuses to
   boot half-seeded. Payload md5s are a different gate: verified
   against the hub manifest at bundle download/export time, and
   pinned by the packaged-skills asset test.

### Hydrate bundle

The interactive frontend ships as a wasm hydrate bundle built from
`crates/mycelium-ui`: two checked-in artifacts under
`crates/mycelium-web/assets/` — `mycelium_ui.js` (the wasm-bindgen JS
wrapper) and `mycelium_ui_bg.wasm` (the compiled module). The JS
wrapper fetches its sibling `.wasm` by filename, so the pair's names
are load-bearing — do not rename. Delivery works like every other
default asset: both files are embedded in the binary and the asset
scaffold writes them to the data-dir assets directory on first boot or
when the assets version marker is stale (an `ASSETS_VERSION` bump
delivers a regenerated bundle to existing deployments; delete the
`.defaults-version` marker to opt out of refreshes). There is no
separate Dockerfile stage — the image ships the bundle through the
same scaffold. `serve_asset` serves the `.wasm` as
`application/wasm`, and the site CSP carries `'wasm-unsafe-eval'`
(WASM modules compile at runtime). The chat and API-keys pages load the
bundle (a `<script type="module">` tag) plus the `islands.js`
traversal script, which initializes the wasm and walks the page's
`<leptos-island>` roots; every other page fetches neither.

Regenerate the bundle whenever `crates/mycelium-ui`'s island code
changes (requires the `wasm32-unknown-unknown` rustup target):

```sh
cargo build -p mycelium-ui --no-default-features --features hydrate \
  --release --target wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.128 --locked   # one-time; pin matters
wasm-bindgen --target web --out-dir /tmp/hydrate-pkg \
  target/wasm32-unknown-unknown/release/mycelium_ui.wasm
cp /tmp/hydrate-pkg/mycelium_ui.js crates/mycelium-web/assets/mycelium_ui.js
cp /tmp/hydrate-pkg/mycelium_ui_bg.wasm crates/mycelium-web/assets/mycelium_ui_bg.wasm
```

Commit the two updated artifacts and bump `ASSETS_VERSION` in both
`crates/mycelium-web/src/assets.rs` and `crates/mycelium-ui/src/shell.rs`
(they must stay equal — a lockstep test enforces it) so running
deployments refresh.