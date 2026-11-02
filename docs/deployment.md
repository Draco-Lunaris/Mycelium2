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
- `GET /api/v1/health` — the minimal authenticated document
  (`status`, `database`, `users`, `version`). The rich posture document
  is the public `/health` above.
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
- Embeddings / vector-index metric lines: `mycelium2_embedding_up`
  (gauge; the same `0`/`1`/`2`/`3` four-state encoding as
  `mycelium2_backend_up`, its own machine — `0` means no stored
  `embeddings` row, i.e. vector retrieval is off),
  `mycelium2_vector_queue_depth` (gauge; write-hook items waiting to be
  embedded) and `mycelium2_vector_processed_total` (counter; concepts
  embedded). `/health` mirrors the state as `embedding_state`,
  `embedding_down_seconds`, `embedding_checked_seconds_ago`. A red
  embeddings backend never makes `/health` degraded either: retrieval
  falls back to lexical and the rows catch up when the model returns.
- Dream metric lines: `mycelium2_dream_ticks_total` (counter; every
  scheduler wake), `mycelium2_dream_passes_total` (counter; wakes that
  ran a pass — the agent ran, found nothing, or failed),
  `mycelium2_dream_passes_skipped_total` (counter; wakes that ended
  without a pass: gate busy, backend red, dreaming disabled, config
  unreadable) and `mycelium2_dream_backfill_active` (gauge; a corpus
  backfill is walking now). `/health` reports the backfill's RESULT as
  `dream_backfill_complete` — the marker's presence, meaning "the most
  recent walk finished" (a fresh click clears it until that walk ends).
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

### Embeddings and the vector index

Two independent background tasks keep the vector half of retrieval
current. Neither one touches the LLM admission gate — embedding traffic
is not an agent run, so it neither occupies nor waits behind a chat
slot.

- **The probe** (30 s cadence, immediate first tick) posts a
  one-element `["probe"]` input to `{base_url}/v1/embeddings` with a
  5 s timeout. Any error counts as one failure — transport, HTTP
  status, an unparseable body, or a vector wider than the 4096-dim
  sanity cap. One or two consecutive failures read **yellow**, three
  or more **red**, and no stored `embeddings` row reads **disabled**
  (grey: nothing to probe, not an outage). Every successful probe also
  re-stamps the model's learned dimensionality, so a model that
  silently changes width is caught within one interval rather than
  cached from first boot.
- **The vector worker** (5 s tick) re-reads the row every tick (absent
  ⇒ the machine goes Disabled and the tick ends), marks every row whose
  model or dims no longer matches the config stale — once, and only
  after a probe has learned a width — skips its embedding phases
  entirely while the backend is red, then drains the **whole**
  write-hook queue and re-embeds those concepts in `batch`-sized
  batches with three attempts per batch. A batch that still fails drops
  with a bounded warning: nothing errors upward, nothing wedges. Finally
  it re-embeds up to 256 stale rows per scope, resuming the rest next
  tick — so a model swap re-embeds a corpus gradually instead of in one
  enormous request.

What that means at the console:

- **Turning embeddings on mid-life needs no restart.** Concept writes
  enqueue their path as they happen and the worker drains only when it
  can read a row, so an operator who saves the config after boot gets
  the pre-existing corpus anyway: the first tick of the process that
  actually sees a row re-enqueues every path lacking a vector row
  (per scope, users included, and a user whose master key is
  temporarily unrecoverable is skipped and resumes at the next boot).
- **The queue is bounded at 512 and drops its newest item at
  capacity.** A full drain runs every tick, so overflow takes a burst of
  more than 512 unserved writes; a dropped concept simply keeps the
  vector it already had until it is written again or the corpus
  re-embeds. Search reads a slightly older vector, never an error, and
  `mycelium2_vector_queue_depth` is where the pressure shows.
- **A red embeddings backend is invisible to users.** Retrieval answers
  lexically, `mycelium2_embedding_up` drops to 1 or 2, and the rows
  catch up on their own when the model returns. After a config save or a
  model change, watch `mycelium2_vector_processed_total` climb and
  `mycelium2_vector_queue_depth` fall back toward zero.
- An **undecryptable or foreign-sealed `embeddings` row is a config
  problem, not weather** — the probe warns each tick and feeds no
  observation, so the dot keeps its last real state (same rule as the
  `llm` row).

### Scheduled dreaming

One worker wakes on the stored `dream_interval_s` (default 86400 s =
daily; 3600–2592000, clamped on save and on read) and runs **one dream
pass per wake**, over a rotation of the maintenance scopes: every user
(`ORDER BY id`) then the `global:skills` shelf, one scope per wake at a
cursor that advances on each attempted pass. A deployment with N users
therefore revisits any given scope roughly every N intervals. The
library is never dreamed — book stacks are ingest output, not knowledge
to consolidate.

- **Admission is try-class, never waiting.** A gate that is busy or
  queued defers the wake: the wake counts as a skip
  (`mycelium2_dream_passes_skipped_total`), the cursor stays, and the
  scope's signals persist for the next wake. Dreaming never takes a slot
  from an interactive chat or query.
- **A red backend defers the wake too** (same accounting — no pass, no
  record, cursor untouched); a yellow one still runs, because a flap is
  not an outage. No usable `llm` row is a guaranteed-fail pass, so the
  wake is skipped before admission rather than burning a slot on it.
- **Unticking `dream_enabled`** on `/admin/dream` stops everything: each
  wake counts a skip, admits nothing, records nothing, and leaves the
  cursor where it was.
- **Cadence changes apply on the next wake** — the scheduler re-reads
  the sealed row each time and re-tunes itself when the value differs. A
  deployment that has never saved the row (no `lifecycle` row at all)
  keeps the built-in daily cadence forever.
- **Each attempted pass writes a `dream_last_run` record** — the scope,
  whether the agent actually ran, and a bounded summary (the agent's own
  summary, the "nothing to do" reason, or the failure detail) — shown on
  `/admin/dream`. An attempted pass is recorded even when it failed,
  including the case where a user's master key was unrecoverable, so the
  readout never silently goes stale.

### Corpus backfill (the `/admin/dream` button)

**Backfill corpus** (`POST /admin/dream/backfill`, admin) walks every
maintenance scope **once** through the same per-scope pass the scheduler
uses, and is how a bundle that has been accumulating signals for months
gets its first consolidation.

- The click deletes the `dream_backfill_at` marker before spawning, so
  `/health` reports `dream_backfill_complete: false` from the instant it
  is launched, and `mycelium2_dream_backfill_active` reads 1 while it
  walks. Exactly one walk runs at a time — a second click flashes
  `Backfill already running`.
- Each scope is admitted try-class, so a busy gate or a red backend
  **skips that scope** and the walk continues to the rest. The marker is
  written when the walk finishes either way, so the flag means "the most
  recent walk completed", not "every scope was consolidated"; a skipped
  scope keeps its signals for the scheduler's rotation or the next walk.
- `dream_enabled` gates nothing here (an explicit admin click is not the
  scheduler), and `dream_last_run` is never written by a backfill — the
  scheduled readout stays the scheduled readout.
- A walk killed by shutdown leaves **no** marker; a walk that finds no
  usable `llm` row, or cannot enumerate users, is abandoned and leaves
  no marker either (nothing ran).

### Revisions: storage and retention

Every concept write appends one row to `concept_revisions` in the same
SQLite database as everything else — no separate directory, nothing new
to mount. Each row carries the **full pre-mutation bytes** of the
concept (`create` rows carry what was created; restoring a path that was
deleted carries no payload), sealed with the scope's index key and bound
by an AAD of `(scope, path token, revision number)`, so a row cannot be
moved between concepts or renumbered without failing decryption.

- **Retention is `revision_retention`** (default 200, 25–2000) and prunes
  in the **same transaction as the insert**: the newest N rows per
  concept survive, the rest are deleted immediately and irreversibly.
  Lowering the setting therefore removes nothing until each concept is
  written again; raising it does not bring pruned rows back.
- **Budget disk accordingly**: rows are full snapshots, not diffs, so a
  concept's history costs roughly `retention × concept size`. Editing
  many large concepts under a high retention is the case that moves the
  database's size.
- **Backups cover history** because it rides the database — see
  [Backup and restore](backup.md); a full restore brings revision
  chains with it.
- **Reading history decrypts nothing.** The listing is metadata only
  (number, op, source, summary, timestamp, and whether the payload is
  empty, judged from the stored envelope's length); viewing a snapshot
  and restoring are the decrypting paths, and a restore writes a **new**
  revision (`restored to revision n`) rather than erasing the chain.
- **Boot order is enforced**: retention is synced from the stored row
  before the packaged-skill seed and every boot sweep, because those
  write revisions that the retention rule must already govern.

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