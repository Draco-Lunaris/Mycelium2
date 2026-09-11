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

1. The server creates the data layout, runs migrations, and
   bootstraps the initial admin.
2. Read the generated password:
   `docker exec mycelium2 cat /opt/mycelium2/data/config/initial-admin-password`
3. Log in at `https://<host>/` — the first login forces a password
   change (the generated password file is not removed automatically;
   delete it after use).

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
  container orchestration; returns only coarse status).
- `GET /api/v1/health` — detailed JSON status (authenticated).
- `GET /metrics` — Prometheus counters (logins, searches, ingests,
  backups, ...) — public by design for scrape endpoints; put it behind
  a firewall or reverse-proxy auth if your threat model requires.
- Container healthcheck: the HTTP redirect listener answers on port
  80 (`curl http://127.0.0.1:80/`).

## Upgrading

The image is data-free: pull the new image and restart. Migrations run
automatically on startup. Take a backup first (see
[Backup and restore](backup.md)).