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
| `mycelium2_memory_query` | Search the caller's private bundle with a natural-language question. Returns ranked results (path, title, snippet, score). Stays private by default — global scopes are not included (that is the web default). |
| `mycelium2_memory_add` | Store new knowledge as an OKF concept (free-form prose in, structured concept out). Returns immediately with a queue receipt — the librarian integrates it in the background; check with `memory_status(receipt_id=…)`. |
| `mycelium2_memory_update` | Correct or extend an existing concept (explicit path, or best search match for the instruction). Returns a queue receipt immediately; check integration with `memory_status(receipt_id=…)`. |
| `mycelium2_memory_status` | Bundle statistics: concept counts, graph health. Pass `receipt_id` to check a queued mutation's state (pending/running/done/dead). |
| `mycelium2_memory_maintain` | Health-check the knowledge graph, then repair it in the background (receipt immediately; check completion with `memory_status`). |
| `mycelium2_skill_get` | Fetch a skill's full markdown by name — private skills first, then the global skills shelf. A bare slug resolves the bundle hub `/{slug}/skill.md` first, falling back to the legacy flat `/{slug}.md`; a name ending in `.md` is taken as an exact bundle path. |
| `mycelium2_skill_list` | List available skills, one entry per logical skill: each nested bundle appears as its hub (`/{slug}/skill.md`); legacy flat skills appear as themselves. The caller's private skills plus global skills. |

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
- **Identity**: the bearer key resolves to a user; all reads/writes
  are scoped to that user's encrypted bundle (plus the global skills
  shelf for the skill tools).
- **Isolation**: two users never see each other's concepts — enforced
  by per-user encryption, not just query filters.
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