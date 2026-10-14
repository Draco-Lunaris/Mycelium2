# TLS Management — Design Spec

Issue: [Draco-Lunaris/Mycelium2#7](https://github.com/Draco-Lunaris/Mycelium2/issues/7)
Date: 2026-10-14
Status: approved (brainstorming complete; decisions recorded below)

## Problem

The auto-generated self-signed certificate has SAN `DNS:localhost` only
(cert.rs:58). Deployments on real hostnames produce hostname-mismatch
errors (`ERR_TLS_CERT_ALTNAME_INVALID`) for every TLS-verifying client
after they trust the cert — unfixable client-side. There is no way to
configure the generated cert's SANs, no way to install a third-party
(ACME / internal-CA / commercial) certificate without shell access
and a compose edit, no CSR generation for obtaining one, and no way to
download the current cert to trust it. `MYCELIUM2_TLS_CERT`/`KEY` env
mounts exist (main.rs:27-32) but are undocumented on the admin side.

## Goals

1. Generated self-signed certs carry the deployment's hostnames.
2. Admins can install third-party certs (upload) and generate CSRs
   through the admin UI, taking effect without a server restart.
3. Admins can download the current certificate for trust distribution.
4. All three TLS postures documented.

## Non-goals

- ACME automation (the CSR + upload flow covers manual/DNS-challenge
  ACME like acme.sh `--dns` manual mode; automated renewal stays out
  of scope for v1).
- Reverse-proxy TLS termination changes — the HTTP→HTTPS redirect
  behavior stays as-is; reverse proxy is a documented posture only.
- Client-trust management on the user side (download link only).

## Verified technical facts

- `axum_server::tls_rustls::RustlsConfig` (0.8.0, in-lockfile) has
  `pub async fn reload_from_pem_file(&self, cert, key) -> io::Result<()>`
  — hot-swap with no restart; internally an ArcSwap-style store, so
  sharing the handle and reloading is race-safe.
- **CORRECTED 2026-10-14 (plan-time API verification):** rcgen 0.13
  DOES have CSR emission — `CertificateParams::serialize_request(
  &KeyPair) -> CertificateSigningRequest` with `.pem()` and
  `.der()`, `distinguished_name.push(DnType::CommonName, …)`, and
  `subject_alt_names: Vec<SanType>` (cert.rs:525, csr.rs:41-57). The
  earlier claim that x509-cert was needed was wrong: CSR building
  stays entirely in rcgen; no new dependency. The x509-cert fallback
  note below is obsolete and kept only as history.
- rcgen 0.13 CAN parse an existing key pair (`KeyPair::from_pem`) and
  exposes `public_key_raw`/`public_key_der` for cert/key match
  validation; `rustls-pemfile` 2.2 is already in the tree for PEM
  framing, and `x509-parser` 0.16 is already transitive (via
  webauthn-rs-core) for cert display parsing — promote it to a direct
  dep of mycelium-web (no version change, no new code in the graph).

## Decisions (user-approved 2026-10-14)

1. **CSR private-key retention**: the pending CSR key stays on disk
   (`config/tls/pending-csr-key.pem`, 0600) until the matching cert is
   uploaded or the admin discards it. (Required — a CSR is useless
   without its key.)
2. **Expiry policy**: warn-only. Uploading an already-expired cert
   succeeds with a clear warning flash showing notAfter; operators
   mid-renewal are the ones uploading, and a hard reject would block
   the recovery path.
3. SANs also as `--tls-sans` CLI flag (clap: env + flag, one line each).
   **AMENDED 2026-10-14 (user ruling, supersedes any earlier text in
   this spec and its plan): the repo is public — no pre-defined
   environment details anywhere.** The SAN set is fully
   operator-defined: when MYCELIUM2_TLS_SANS/--tls-sans is set, the
   list is taken verbatim (dedup, order-stable); `localhost` is only
   the DEFAULT when the value is unset/empty — never force-added to a
   configured set. All test fixtures use RFC documentation values
   (mycelium2.example, 192.0.2.10), never real hostnames/IPs.
4. Env `MYCELIUM2_TLS_SANS` is boot-time (matches the
   `MYCELIUM2_WEBAUTHN_ORIGIN` precedent — cert built at boot, before
   any request exists).

## Architecture

### New module `crates/mycelium-web/src/tls_admin.rs`

Owns: cert display parsing, upload validation, CSR generation, the
pending-CSR lifecycle, and the on-disk TLS layout. The `RustlsConfig`
handle moves into `AppState` (Arc) at boot; all reloads go through it.

### On-disk layout under `<data_dir>/config/tls/`

```
cert.pem              # generated self-signed pair (unchanged)
key.pem               #   0600
sans.txt              # SANs the generated cert was built with
uploaded-cert.pem     # third-party chain (most recent successful)
uploaded-key.pem      #   0600
uploaded.json         # { label, uploaded_at, fingerprint_sha256, source: "uploaded" }
pending-csr-key.pem   # 0600; key for the outstanding CSR
pending.json         # { cn, sans, created_at }
```

### Boot precedence (cert.rs extension)

1. `MYCELIUM2_TLS_CERT`/`KEY` env mounts (unchanged priority 1).
2. `uploaded-cert.pem` + `uploaded-key.pem` if both exist — an
   uploaded cert survives restarts without env config.
3. Generated pair; regenerate if `sans.txt` differs from the
   requested SAN set (env/flag + always-localhost). Regeneration
   under this step still happens when an uploaded pair is active —
   the generated pair is the fallback and stays current; the
   uploaded pair remains the served cert.

### `AppState` changes

`AppState::new` gains a `tls: Option<Arc<RustlsConfig>>` field
(boot-created before serving; `Option` so existing tests that don't
serve TLS keep working — `None` disables the admin TLS reload calls
with a clear "restart required" flash). The serve path passes the same
Arc into `bind_rustls`.

### Endpoints (all admin-gated + CSRF; forms flash their verdicts, PEM downloads stream files)

- `GET /admin/tls` — TLS card (details below) rendered into the
  existing admin System page section.
- `GET /admin/tls/cert` — the active cert PEM (uploaded if active,
  else generated). `application/x-pem-file`, `no-store`. Key never
  served.
- `POST /admin/tls/upload` (multipart) — fields `cert_pem` (file),
  `key_pem` (file), `label` (text, optional). Validation pipeline
  (all BEFORE any write): PEMs parse; cert/key pair matches; if a
  pending CSR exists, cert matches the pending key (exact-match flow)
  or is a fresh pair; expiry computed (warn-only flash). On success:
  write uploaded-*.pem + uploaded.json, `tls.reload_from_pem_file`,
  clear pending-*. On any failure: nothing written, flash the exact
  parse/mismatch error. Size cap mirrors the book-upload limit.
- `POST /admin/tls/csr` (form: `cn`, `sans` comma-separated) —
  generates ECDSA P-256 key, builds CSR (x509-cert), serves the CSR
  PEM as a download response AND persists pending-csr-key.pem +
  pending.json. Replaces any prior pending CSR (last-one-wins, flash
  says so).
- `GET /admin/tls/csr` — re-download the outstanding pending CSR PEM
  (flash-not-found on the card when none is pending).
- `POST /admin/tls/pending/discard` — clears pending-* (button on the
  card when a pending CSR exists).
- `POST /admin/tls/reset` — deletes uploaded-*.pem/json and reloads
  from the generated pair (rollback from a bad upload; flash
  confirms; the generated pair is never deleted by this action).

### Admin System page TLS card

- Current cert: subject, issuer, SANs, notBefore/notAfter, SHA-256
  fingerprint, source (generated / uploaded with label), and expiry
  state (ok / expiring-in-≤30d warn / expired warn).
- Pending CSR section (when present): CN, SANs, created_at, a
  re-download button (`GET /admin/tls/csr`), and Discard.
- Forms: Upload cert/key (multipart), Generate CSR (cn + sans).
- Download cert link.
- All values XSS-escaped via the established page patterns.

### CSR building

`x509-cert` `request::Request` builder: subject RDN (CN from form),
SAN extension from the form's SAN list (localhost always included,
mirroring the generated-cert merge), ECDSA P-256 via the
already-present `p256` transitive dep (verify direct dep needed — add
`p256` explicitly if required for signing). CSR PEM served; key
persisted 0600.

## Error handling

- Every failure surfaces as a flash with the exact reason (PEM parse
  error text, key-mismatch, io error); internal crypto failures are
  logged server-side with the generic-massage convention where user
  detail isn't actionable.
- Validation-before-write everywhere: no partial state from a failed
  upload.
- `AppState.tls == None` (tests): reload-dependent actions flash
  "restart required" instead of failing silently.

## Testing

- Unit: SAN env parse/merge; sans.txt sidecar regen logic; PEM
  validation pipeline (valid pair, mismatched key, garbage PEM);
  CSR structure (re-parse emitted CSR: subject CN + SANs round-trip);
  expiry classification.
- Integration: upload a generated test pair via multipart → active
  cert changes + flash fingerprint; upload with mismatched key →
  error flash, uploaded files unchanged; CSR endpoint → parseable
  CSR + pending files present; discard; reset; `GET /admin/tls/cert`
  (admin 200 / unauth 303); boot precedence (uploaded beats
  generated; env beats uploaded).
- Standard gate: cargo check / fmt --check / clippy -D warnings /
  test --workspace.

## Documentation

`docs/tls.md`: the three postures (env-SAN self-signed; uploaded
third-party incl. the CSR→CA→upload walkthrough; reverse-proxy
termination), cert trust instructions (NODE_EXTRA_CA_CERTS, system
trust store), `MYCELIUM2_TLS_SANS` env + `--tls-sans` flag, and the
uploaded-cert boot precedence. Admin UI text links to it.

## Open implementation notes

- If `x509-cert`'s API for SAN-in-CSR proves awkward, fall back to
  DER-assembly from `der` crate primitives (still RustCrypto). Either
  path stays pure-Rust — no openssl-binary dependency in the
  container.
- `p256` may need to move from transitive to explicit dependency for
  the CSR signing path.
- Hot-reload is verified for new connections only (rustls stores
  configs per-connection at accept) — acceptable; documented in
  docs/tls.md.