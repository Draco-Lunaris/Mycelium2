# Backup and restore

## What to back up

The entire data directory (default `/opt/mycelium2/data`):

- `config/` — **service key** (encrypts all global data), initial
  admin secrets, JWT signing keys
- `db/` — SQLite metadata (users, sessions, index tokens, bookshelves)
- `users/` — per-user encrypted concept files
- `library/` — shared encrypted stacks (book texts + catalogs)
- `skills/` — global skills shelf
- `assets/` — static assets (recreatable, but included for
  completeness)

**Losing `config/service-key` makes all global-scope data (bookshelves,
library, global skills) unrecoverable.** Per-user data additionally
needs each user's password (or recovery key) — that is by design.

## Taking backups

### Admin portal
**Admin → Maintenance → Download backup** streams a `tar.gz` of the
data directory. This is a best-effort snapshot of a live database.

### CLI (recommended for scheduled backups)

```sh
mycelium2-cli backup --data-dir /opt/mycelium2/data --out /backups/myc2.tar.gz
```

For a guaranteed-consistent snapshot, stop the server first:

```sh
docker compose stop
mycelium2-cli backup --data-dir /opt/mycelium2/data --out /backups/myc2.tar.gz
docker compose start
```

### Verify a backup

```sh
mycelium2-cli verify --data-dir /opt/mycelium2/data
```

Checks the directory layout and runs SQLite `PRAGMA integrity_check`.

## Restoring

1. Stop the server.
2. Extract the archive over the data directory:
   `tar -xzf myc2.tar.gz -C /opt/mycelium2/data`
3. Fix ownership if needed (`chown -R mycelium:mycelium`).
4. Start the server — migrations run automatically.

## Recovery keys

Each user gets a **recovery key** at account creation (shown once to
the admin). It is the only self-service path if a password is lost —
admins cannot read user data. Store recovery keys out-of-band
(password manager, printed vault).

## Migration from the original Mycelium (TypeScript)

- OKF bundles are compatible: markdown concepts can be imported
  as-is (per-user: upload through the concept editor or API).
- `book://` anchor semantics are preserved (128k passage cap).
- Existing global shelf content can be re-uploaded into a
  global-read bookshelf via the librarian.