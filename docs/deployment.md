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
  has waited past twice the integration deadline.
- `GET /api/v1/health` — detailed JSON status (authenticated).
- `GET /metrics` — Prometheus counters (logins, searches, ingests,
  backups, mutation-queue totals, ...) plus gauges for the mutation
  queue depth, the oldest pending item's age, and
  `mycelium2_llm_last_success_timestamp` (last successful LLM
  completion; absent until the first success) — public by design for
  scrape endpoints; put it behind a firewall or reverse-proxy auth if
  your threat model requires.
- Container healthcheck: the HTTP redirect listener answers on port
  80 (`curl http://127.0.0.1:80/`).

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