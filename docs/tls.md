# TLS configuration

Mycelium2 always serves HTTPS; HTTP on the second listener only
redirects. There are three supported postures, plus one knob for the
built-in certificate's names.

## Boot precedence

At startup the server picks its certificate in this order:

1. **Env-supplied pair** — `MYCELIUM2_TLS_CERT` + `MYCELIUM2_TLS_KEY`
   (or `--tls-cert` / `--tls-key`) when both files exist. The server
   loads them and touches nothing else.
2. **Uploaded pair** — `config/tls/uploaded-cert.pem` +
   `uploaded-key.pem` in the data directory, installed through
   Admin → System → TLS. Survives restarts. An unreadable uploaded
   pair logs a warning and falls back to the generated pair rather
   than failing boot.
3. **Generated pair** — `config/tls/cert.pem` + `key.pem`, a
   self-signed certificate Mycelium2 creates on first run and keeps
   current (see SANs below). The generated pair is maintained even
   while an uploaded pair is active, so reverting always has a
   working fallback.

The generated pair is **managed** with the sidecar as the source of
truth: `config/tls/sans.txt` records the SAN set it was built for.
Once that sidecar exists and the pair is complete, boot never
regenerates the pair — the env/flag SAN list is only the FIRST-BOOT
initializer. This is what makes the admin UI's **Regenerate
self-signed certificate** action survive restarts (see below): the
recorded set wins over whatever the environment says on a later boot.
A hand-placed `config/tls/cert.pem` + `key.pem` pair that predates
this version — one you dropped in yourself, with no `sans.txt` beside
it — is silently deleted and regenerated on the first boot after
upgrade. If the pair is incomplete (a crash between writes), boot
rebuilds it from the **recorded** set. Removing `sans.txt` is the
escape hatch that hands control back to the env/flag initializer.
Real certificates do not belong in that directory at all: mount them
via `MYCELIUM2_TLS_CERT` / `MYCELIUM2_TLS_KEY` (precedence 1) or
install them through the upload flow (precedence 2), neither of which
the manager ever touches.

## Posture 1: built-in self-signed (with your names)

With no configuration the generated certificate carries the SAN
`localhost`. To get your own names in the self-signed cert, set the
SAN list:

```sh
MYCELIUM2_TLS_SANS="mycelium.example,192.0.2.10,localhost" mycelium2
# or
mycelium2 --tls-sans "mycelium.example,192.0.2.10,localhost"
```

The list is comma-separated and taken **verbatim** — nothing is
force-added, so include `localhost` if you also browse via the loopback
name. An unset or empty value means exactly `localhost`. The env/flag
list is the **first-boot initializer**: the server records it in
`config/tls/sans.txt`, and from then on the sidecar — not the
environment — owns the generated pair (see Boot precedence above). To
change the names later, either regenerate from the admin UI
(below) or delete `sans.txt` and restart. The uploaded pair is never
regenerated or touched by any of this.

## Posture 2: uploaded third-party certificate

Admin → System → TLS is the whole workflow:

- **Upload certificate** — a cert (+ optional issuer chain, leaf
  first) and its private key, with an optional label. Everything is
  validated before anything is written: both PEMs parse, the key
  matches the certificate, and — when a CSR is pending — the uploaded
  key must be exactly the key the CSR was generated with. An expired
  or soon-to-expire certificate installs with a warning rather than a
  rejection. On success the live listener hot-swaps to the new pair
  (new connections pick it up immediately; existing connections keep
  their negotiated parameters).
- **Download certificate** — the active cert PEM, for distributing to
  clients that need to trust it.
- **Revert to generated** — removes the uploaded pair and reloads the
  generated one. The generated pair is never deleted by this action.
- **Regenerate self-signed certificate** — mints a fresh self-signed
  key and certificate with the given SAN list (comma-separated; empty
  means `localhost`; at most 64 names of 253 characters each), even
  when the SANs are unchanged — the action is
  never a silent no-op. The live listener hot-swaps to the fresh pair
  when no uploaded cert is active; under an active upload the fresh
  pair becomes the fallback and the uploaded cert keeps serving. The
  choice is recorded in `sans.txt` and survives restarts (see Boot
  precedence). Clients that trusted the old certificate must re-trust
  the new one — its fingerprint changes.

### CSR → CA → upload walkthrough

If your CA wants a certificate signing request, generate one on the
same card: **Generate CSR** takes a common name and a SAN list
(comma-separated; empty means `localhost`), downloads the CSR PEM, and
keeps the matching private key server-side (mode 0600) as *pending*.
While a CSR is pending, uploads must carry exactly that key — the card
shows the pending CN/SANs and offers re-download and discard.

Have acme.sh issue via manual DNS (no account or automation needed):

```sh
acme.sh --issue --dns dns_yourprovider \
  -d mycelium.example \
  -d www.mycelium.example \
  --csr /path/to/csr.pem
```

Or sign with an internal CA using openssl (create a v3 ext file
carrying the SANs you requested, then):

```sh
openssl ca -batch -config ca.cnf -extensions server \
  -in csr.pem -out issued.pem
```

Upload `issued.pem` (plus your chain, leaf first) through the card
together with the pending key — the upload form always takes both a
certificate and a key, and the key must be exactly the one the CSR was
made from. That key never leaves the server: read it off disk at
`<data_dir>/config/tls/pending-csr-key.pem` (the card prints the same
path) and pick it as the **Private key (PEM)** file. A successful
install clears the pending CSR. Discarding a pending CSR is fine — it
just means the next upload is an independent pair, and you supply both
halves yourself.

## Posture 3: reverse proxy terminates TLS

Run your proxy (nginx, Caddy, …) with your real certificate in front of
Mycelium2. Mycelium2 always terminates its own TLS, so the proxy must
forward to the **HTTPS** listener, not the HTTP one — the HTTP listener
is redirect-only (it 303s every request to the HTTPS port), so proxying
to it either loops or bypasses your proxy entirely. Bind Mycelium2 to
loopback and point the proxy at it over TLS:

```
MYCELIUM2_HTTPS_ADDR=127.0.0.1:8443
```

```nginx
# nginx: terminate TLS here, proxy to Mycelium2's HTTPS listener
location / {
    proxy_pass https://127.0.0.1:8443;
    proxy_ssl_verify on;  # proxy_ssl_trusted_certificate has no effect without this
    proxy_ssl_trusted_certificate <data_dir>/config/tls/cert.pem;  # the self-signed pair, or set proxy_ssl_verify off
}
```

(See [Deployment](deployment.md) — *Behind a reverse proxy* — for the
full `proxy_pass https://…` recipe.) Two things to get right:

- The proxy connects to Mycelium2 over TLS, so it must trust (or be told
  to skip verifying) the backend certificate — the self-signed pair from
  Admin → System → TLS, or the `MYCELIUM2_TLS_CERT`/`MYCELIUM2_TLS_KEY`
  pair if you mounted one.
- Pass `MYCELIUM2_WEBAUTHN_ORIGIN` as the public HTTPS origin your
  browser actually sees (e.g. `https://mycelium.example`) — passkey
  ceremonies are refused when the relying-party origin does not match.

## Trusting the self-signed certificate

Downloading the cert from Admin → System → TLS gives you the file;
clients that validate hostnames will still reject it unless they trust
it (or the SANs do not cover the name you browse by — issue #7's
background, which `MYCELIUM2_TLS_SANS` fixes).

Node-based clients (including MCP clients) trust extra CAs via:

```sh
export NODE_EXTRA_CA_CERTS=/path/to/mycelium-cert.pem
```

On Debian/Ubuntu, install it into the system trust store:

```sh
sudo cp mycelium-cert.pem /usr/local/share/ca-certificates/mycelium.crt
sudo update-ca-certificates
```

curl and Python `requests` read the system store; Python `httpx` and
most tools also accept an explicit CA path (e.g. `--cacert`,
`verify=`).

## Hot reload scope

Uploading or reverting reloads the live TLS config, which applies to
**new connections** — rustls picks up the config per-connection at
accept time. Long-lived keep-alive connections finish on the old
parameters; nothing forces them down.
