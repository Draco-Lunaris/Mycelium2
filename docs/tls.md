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
name. An unset or empty value means exactly `localhost`. The server
records the requested set in `config/tls/sans.txt`; when the set
changes across restarts it regenerates the generated pair (the
uploaded pair is never regenerated or touched).

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

Upload `issued.pem` (plus your chain, leaf first) and the pending key
through the card; a successful install clears the pending CSR.
Discarding a pending CSR is fine — it just means the next upload is an
independent pair.

## Posture 3: reverse proxy terminates TLS

Run your proxy (nginx, Caddy, …) with your real certificate in front of
Mycelium2. Mycelium2 always terminates its own TLS, so the proxy must
forward to the **HTTPS** listener, not the HTTP one — the HTTP listener
is redirect-only (it 307s every request to the HTTPS port), so proxying
to it either loops or bypasses your proxy entirely. Bind Mycelium2 to
loopback and point the proxy at it over TLS:

```
MYCELIUM2_HTTPS_ADDR=127.0.0.1:8443
```

```nginx
# nginx: terminate TLS here, proxy to Mycelium2's HTTPS listener
location / {
    proxy_pass https://127.0.0.1:8443;
    proxy_ssl_trusted_certificate /etc/mycelium2/cert.pem;  # the self-signed pair, or set proxy_ssl_verify off
}
```

(See [deployment.md](deployment.md) — *Behind a reverse proxy* — for the
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
