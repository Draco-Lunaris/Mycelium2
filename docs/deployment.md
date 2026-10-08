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
2. The packaged `pdf-to-markdown` skill — PDF book → enhanced markdown
   ready for library ingest — is seeded into the global skills shelf
   automatically. Every user can read it from `/skills`; admins can
   edit. Upgrades refresh it when the packaged version changes; admin
   edits are preserved between upgrades.
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

### Packaged skills (seed v2)

The packaged `pdf-to-markdown` skill seeds as a nested bundle
(`SKILLS_SEED_VERSION` 2):

1. Fresh boots write the nested layout directly: the
   `/pdf-to-markdown/skill.md` hub (carrying the bundle manifest),
   its companion concepts under `/pdf-to-markdown/`, and the scripts
   as raw payload files — scripts are never concepts (no registry
   row, no search entry).
2. A shelf seeded by an older release migrates on the first boot of
   the new binary: the legacy flat `/pdf-to-markdown*.md` concepts
   are deleted (idempotently) and the nested layout is written.
   Admin edits to the legacy flat packaged paths do not survive this
   one-time migration — copy them out first if you modified them.
3. The `.seed-version` marker gates refresh as before: a matching
   marker is a no-op (admin edits to the packaged paths survive
   between bumps), and extra admin-created skills are never touched.
   Seeding fails fast on a bad embedded manifest or md5 — the server
   refuses to boot half-seeded.