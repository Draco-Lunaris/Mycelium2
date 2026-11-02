# MCP integration

Mycelium2 exposes a **stateless MCP 2026-07-28** server over
Streamable HTTP at `https://<host>/mcp`. AI agents connect with a
per-user API key as the bearer token; every request is authenticated
independently (no sessions).

## Setup

1. Log in to the web UI → **API Keys** → mint a key (`myc2-...`,
   shown once).
2. Point your MCP client at the server:

```json
{
  "mcpServers": {
    "mycelium2": {
      "type": "http",
      "url": "https://mycelium.example.com/mcp",
      "headers": {
        "Authorization": "Bearer myc2-..."
      }
    }
  }
}
```

The server advertises only protocol version `2026-07-28` and requires
per-request metadata (stateless mode).

## Tools

| Tool | Description |
|---|---|
| `mycelium2_memory_query` | Answer a natural-language question from the caller's bundle plus the global shelves — ranked hits by default, agent synthesis on request. Full contract below. |
| `mycelium2_memory_add` | Store new knowledge as an OKF concept (free-form prose in, structured concept out). Returns immediately with a queue receipt — the librarian integrates it in the background; check with `memory_status(receipt_id=…)`. |
| `mycelium2_memory_update` | Correct or extend an existing concept (explicit path, or best search match for the instruction). Returns a queue receipt immediately; check integration with `memory_status(receipt_id=…)`. |
| `mycelium2_memory_status` | Bundle statistics: concept counts, graph health. Pass `receipt_id` to check a queued mutation's state (pending/running/done/dead). |
| `mycelium2_memory_maintain` | Health-check the knowledge graph, then repair it in the background (receipt immediately; check completion with `memory_status`). |
| `mycelium2_skill_get` | Fetch a skill's full markdown by name — private skills first, then the global skills shelf. A bare slug resolves the bundle hub `/{slug}/skill.md` first, falling back to the legacy flat `/{slug}.md`; a name ending in `.md` is taken as an exact bundle path. |
| `mycelium2_skill_list` | List available skills, one entry per logical skill: each nested bundle appears as its hub (`/{slug}/skill.md`); legacy flat skills appear as themselves. The caller's private skills plus global skills. |

### `mycelium2_memory_query`

| Argument | Default | Meaning |
|---|---|---|
| `question` | required | Natural-language question. Tokens are the search terms; **32 terms max** (the encrypted index does one lookup per term), so a pasted document is truncated to its first 32 words rather than amplifying into a huge query |
| `limit` | admin `query_top_k` (8) | Maximum hits, hard-capped at 64. `0` is honored as "no hits" |
| `synthesize` | `false` | `true` runs the librarian agent (search → read → answer) and returns a grounded answer instead of a hit list |
| `include_inactive` | `false` | `true` also returns `retired` and `superseded` concepts — the history-query opt-in. Default retrieval never shows them |

**What it reads.** The caller's private bundle, the global skills shelf,
and the library catalogs whose bookshelves are visible to this caller
(admins see private shelves; other users see global-read ones). The
visibility rules are the same ones the web search applies — the two
surfaces call one shared composer, so a hit list here and a search there
agree.

**How it is retrieved.** The same hybrid pipeline as
[`GET /api/v1/search`](api.md): per scope, the lexical top-50 fuses with
the vector top-50 by reciprocal-rank fusion when an embeddings backend
is configured and healthy, and the fused scores sort across scopes. When
the embeddings layer is absent, red, or unreachable the result is the
lexical one, silently — a vector outage never fails a query.

**Three ways it can answer:**

1. `synthesize: false` (default) — zero LLM calls, ranked hits only.
2. `synthesize: true` with an LLM configured — the agent loop, admitted
   on the shared gate with the short `query_wait_secs` bound, its answer
   cached (an identical repeat is answered from the cache without
   waiting for the gate at all).
3. `synthesize: true` with no LLM, an unreachable one, or no slot in time
   — the deterministic hit list, i.e. case 1. **Capacity degrades the
   answer, it never errors it.**

**Hit shape** (each of the ten fields; a hit whose concept could not be
re-read keeps its place with the unreadable fields as `null`):

```json
[{"scope": "user", "path": "/rust/async-basics.md", "title": "Async basics",
  "description": "…", "snippet": "…", "score": 0.0328,
  "timestamp": "2026-09-30T00:00:00Z", "confirmed_at": "2026-10-14T00:00:00Z",
  "status": "active", "superseded_by": null}]
```

`score` is the RRF fusion value, not a BM25 magnitude. `confirmed_at`,
`status` and `superseded_by` are the lifecycle frontmatter, so a client
can see for itself that a hit is current — and with
`include_inactive: true`, tell a superseded concept from its replacement
by following `superseded_by`.

## Skills

A skill uses the nested bundle layout: `/<slug>/skill.md` is the hub
(a `type: Skill` concept whose frontmatter carries the bundle
manifest — file roles and md5s), companion concepts under `/<slug>/`
are its docs, and scripts are raw payload files, never concepts. So
`skill_list` returns one entry per skill (the hub; companions and
scripts never list), and `skill_get` resolves a bare slug to the hub
first. To install a skill on a machine, download the bundle zip from
`GET /api/v1/skills/{slug}/bundle` (see [API](api.md)) or export a
directory tree with `mycelium2-cli skill export`.

## Semantics

- **Deferred writes**: memory_add/update/maintain return a receipt; integration by the
  librarian happens in the background (deterministic fallback if the LLM is down — items
  are only lost if they exceed the retry deadline, never silently). Receipts survive
  restarts.
- **Identity**: the bearer key resolves to a user; writes and private
  reads are that user's encrypted bundle and nothing else, and reads may
  additionally see the global shelves (skills, and the library shelves
  this user is allowed to see) — never another user's concepts.
- **Isolation**: two users never see each other's concepts — enforced
  by per-user encryption, not just query filters.
- **Lifecycle**: `retired` and `superseded` concepts are invisible to
  ordinary retrieval (the same active-only rule the web search and the
  vector index use); `include_inactive: true` is the history query.
  Writes made here are **enqueue-only** and land as ordinary revisions,
  so the dream's consolidation signals count them.
- **Revocation**: revoking the key (API Keys page) immediately
  blocks MCP access.
- **Errors**: expected failures (not found, invalid input) surface as
  readable messages; internal errors are logged and reduced to a
  generic message (no internal detail leaks).

## Example call

```sh
curl -s https://mycelium.example.com/mcp \
  -H "Authorization: Bearer myc2-..." \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call",
       "params":{"name":"mycelium2_memory_query",
                 "arguments":{"question":"deployment runbook"}}}'
```