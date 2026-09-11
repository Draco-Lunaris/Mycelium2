# REST API (`/api/v1`)

All endpoints require authentication: a session cookie (browser) or an
API key as `Authorization: Bearer myc2-...` (machine). Browser writes
additionally require the CSRF token (header `x-csrf-token` or form
field `csrf_token`); bearer requests are CSRF-exempt.

## Concepts

### `GET /api/v1/concepts/{*path}`
Fetch one concept's markdown (`text/markdown`). Paths are bundle-
relative with a leading slash, e.g. `/api/v1/concepts/notes/todo.md`.

### `PUT /api/v1/concepts/{*path}`
Create/replace a concept. Body: `{"markdown": "---\ntype: Note\n..."}`.
The frontmatter must parse and `type` is required. `201` on success.

### `DELETE /api/v1/concepts/{*path}`
Delete a concept. `204` on success.

## Graph and search

### `GET /api/v1/graph`
The caller's private-bundle graph: `{"nodes": [...], "edges": [...]}`.
(Global scopes are never included — graph view is private by design.)

### `GET /api/v1/search?q=...&global=1`
Search the caller's bundle; `global=1` adds the global skills shelf
and library catalogs. Results are scope-tagged:

```json
[{"concept_path": "/my-book/book.md", "title": "My Book",
  "snippet": "...", "score": 3.0, "scope": "library"}]
```

`scope` is `"user"`, `"skills"`, or `"library"`. Library hits from
admin-private bookshelves are filtered out for non-admin callers.

## Books and passages

### `POST /api/v1/ingest` *(admin)*
Multipart book upload: fields `bookshelf` (name), `slug`, `title`,
`file` (markdown, ≤ the configured limit — default 32 MiB, admin-adjustable), plus the CSRF token. Runs the ingest
inline and returns `202`:

```json
{"job_id": "...", "book_id": "...", "slug": "my-book",
 "status": "done", "detail": "3 catalog concepts written"}
```

Errors: `400` unknown bookshelf/invalid input, `409` duplicate slug or
user already has a job running.

### `GET /api/v1/ingest` *(admin)*
List the 50 most recent ingest jobs.

### `GET /api/v1/ingest/{id}` *(admin)*
One job's status: `{"id": "...", "status": "done", "detail": "..."}`.

### `GET /api/v1/passages?resource=book://<slug>%23<anchor>`
Read a passage from the shared library stacks. Anchors:
`ch-<n>-<slug>` (chapter) or `sec-<n>-<m>-<slug>` (section); an empty
anchor returns the whole text (truncated at 128k chars). The book's
shelf must be global-read (or the caller is an admin).

## Health

### `GET /api/v1/health`
Detailed JSON health (authenticated). `GET /health` (coarse status)
and `GET /metrics` (Prometheus text) live outside `/api/v1` and are
public by design — for container orchestration and scrape endpoints.

## Error shape

Errors are `{"error": "..."}` with an appropriate status code.
Internal details (SQL, paths, crypto errors) are never included.