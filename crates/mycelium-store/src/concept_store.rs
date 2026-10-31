//! Concept store: the coordinated facade over FileRepo + EncryptedIndex +
//! the scope_files registry + the revision undo log — one place that keeps
//! files, index, and listing metadata in sync (the Phase 3 review's
//! "atomic lifecycle" gap), and the write-ahead revision boundary where
//! every mutation records its undo row first (spec §3.2).

use chrono::Utc;
use mycelium_core::concept::Concept;
use mycelium_crypto::keys::{MasterKey, ServiceKey};
use uuid::Uuid;

use crate::Store;
use crate::file_repo::{FileRepo, FileRepoError, Scope};
use crate::revisions::{RevisionStore, RevisionsError, op_summary};
use crate::search_index::{EncryptedIndex, IndexError};

#[derive(Debug, thiserror::Error)]
pub enum ConceptStoreError {
    #[error("file repo error: {0}")]
    Repo(#[from] FileRepoError),
    #[error("index error: {0}")]
    Index(#[from] IndexError),
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("concept parse error: {0}")]
    Parse(#[from] mycelium_core::ConceptError),
    #[error("revision log error: {0}")]
    Revisions(#[from] RevisionsError),
    #[error("concept not found: {0}")]
    NotFound(String),
    #[error("{0} is a reserved filename (index.md, log.md, info.md) — system-maintained")]
    Reserved(String),
    #[error("{0} is reserved for the mutation queue — use the enqueue API")]
    QueuePath(String),
    #[error("queue error: {0}")]
    Queue(#[from] crate::QueueError),
}

/// A listed concept (from the registry, no decryption needed).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ConceptEntry {
    pub path: String,
    pub title: String,
    pub concept_type: String,
    pub updated_at: String,
}

/// Which scope a ConceptStore operates on.
#[derive(Debug, Clone)]
pub enum ConceptScope {
    User {
        user_id: Uuid,
        master_key: MasterKey,
    },
    Service {
        service_key: ServiceKey,
        /// Registry/index namespace: "skills" or "library".
        namespace: String,
    },
}

impl ConceptScope {
    /// The registry/index namespace id (`user:<uuid>` / `global:<ns>`).
    pub fn scope_id(&self) -> String {
        match self {
            ConceptScope::User { user_id, .. } => format!("user:{user_id}"),
            ConceptScope::Service { namespace, .. } => format!("global:{namespace}"),
        }
    }

    fn repo_scope(&self) -> Scope {
        match self {
            ConceptScope::User { master_key, .. } => Scope::User(master_key.clone()),
            ConceptScope::Service { service_key, .. } => Scope::Service(service_key.clone()),
        }
    }
}

/// Coordinated concept storage for one scope.
pub struct ConceptStore<'a> {
    store: &'a Store,
    scope: ConceptScope,
    repo: FileRepo,
    index: EncryptedIndex,
    /// The scope's revision chain (write-ahead undo log keyed off the
    /// same pool + scope keys — spec §3.2).
    revisions: RevisionStore<'a>,
    /// Provenance label stamped into `frontmatter.source` on every put
    /// (e.g. `mcp:<key-label>`, `web:<username>`, `ingest`, `seed`).
    /// `None` preserves the author's own frontmatter (plain reads/tests).
    provenance: Option<String>,
    /// Explicit revision-op tag for every revision a put writes
    /// (`migration` for the boot sweep; `merge`/`consolidate` reserved
    /// for step 5's dream sites). `None` derives op mechanically.
    revision_op: Option<String>,
}

impl<'a> ConceptStore<'a> {
    /// The scope's registry id (`user:<uuid>` / `global:<ns>`) — used by
    /// cache fingerprints and trace sinks.
    pub fn scope_id(&self) -> String {
        self.scope.scope_id()
    }

    /// Label this store's writes with a provenance source (system
    /// provenance — the write-time stamp overwrites whatever the
    /// caller's frontmatter carries in `source`). Consuming builder;
    /// chain after `for_user`/`for_service`. An empty label is treated
    /// as unlabeled (legacy queue rows predate the source column).
    pub fn with_provenance(mut self, source: impl Into<String>) -> Self {
        self.provenance = Some(source.into());
        self
    }

    /// Tag every revision a put writes with this op — the explicit tag
    /// for system-driven rewrites (the deprecated-tag boot sweep tags
    /// its writes `migration`; step 5's dream sites tag `merge`/
    /// `consolidate`). Consuming builder; chain after `for_user`/
    /// `for_service`. Leave the tag off for ordinary writes: the op then
    /// derives mechanically (scope_files existence ⇒ create/update). The
    /// vocabulary is [`crate::revisions::REVISION_OPS`] — an unknown tag
    /// fails the put loudly at record time, before any effect.
    pub fn with_revision_op(mut self, op: impl Into<String>) -> Self {
        self.revision_op = Some(op.into());
        self
    }

    /// Open the concept store for a user's private bundle.
    pub fn for_user(store: &'a Store, user_id: Uuid, master_key: MasterKey) -> Self {
        let repo = FileRepo::new(store.user_dir(user_id));
        let index =
            EncryptedIndex::for_user(store.pool().clone(), &user_id.to_string(), &master_key);
        // Built before the key moves into the scope enum.
        let revisions = RevisionStore::for_user(store.pool(), user_id, &master_key);
        Self {
            store,
            scope: ConceptScope::User {
                user_id,
                master_key,
            },
            repo,
            index,
            revisions,
            provenance: None,
            revision_op: None,
        }
    }

    /// Open the concept store for a service-key scope (global skills
    /// shelf, shared library stacks). `namespace` separates the two
    /// service scopes in the registry and index: `"skills"` and
    /// `"library"` — their listings and search results never mix.
    pub fn for_service(
        store: &'a Store,
        service_key: ServiceKey,
        dir: &std::path::Path,
        namespace: &str,
    ) -> Self {
        let repo = FileRepo::new(dir);
        let index = EncryptedIndex::for_service(store.pool().clone(), &service_key, namespace);
        let revisions = RevisionStore::for_service(store.pool(), &service_key, namespace);
        Self {
            store,
            scope: ConceptScope::Service {
                service_key,
                namespace: namespace.to_string(),
            },
            repo,
            index,
            revisions,
            provenance: None,
            revision_op: None,
        }
    }

    /// Store a concept: record its revision (write-ahead undo row — the
    /// boundary hook, spec §3.2), then encrypt the file, register it,
    /// and index it, then regenerate the scope's index.md. Ordering:
    /// revision → file → registry → index; a failing revision insert
    /// fails the put before anything moves. The registry is the source
    /// of truth for listing; the index lags it, never leads. A crash
    /// between steps leaves an orphaned encrypted file or a
    /// listed-but-unsearchable concept (visible, self-heals on next
    /// put) — never a search ghost.
    ///
    /// Reserved filenames (`index.md`, `log.md`, `info.md`) are
    /// rejected — they are system-maintained (index.md is regenerated
    /// on every write) and a concept there would be silently clobbered.
    pub async fn put(&self, concept: &Concept) -> Result<(), ConceptStoreError> {
        if concept.source_path.starts_with("/mutation-queue/") {
            return Err(ConceptStoreError::QueuePath(concept.source_path.clone()));
        }
        self.put_batch(std::slice::from_ref(concept)).await
    }

    /// Batched put: writes many concepts with ONE index.md regeneration
    /// at the end (put() regenerates per concept — O(n²) on a
    /// 300-chapter catalog ingest). Same ordering guarantees as put():
    /// revision → file → registry → index per concept (each writes its
    /// own revision row ahead of its effect), then the single regen.
    ///
    /// Rejects the mutation queue's reserved `/mutation-queue/` prefix —
    /// only the queue's internal bypass may write there.
    pub async fn put_batch(&self, concepts: &[Concept]) -> Result<(), ConceptStoreError> {
        for concept in concepts {
            if concept.source_path.starts_with("/mutation-queue/") {
                return Err(ConceptStoreError::QueuePath(concept.source_path.clone()));
            }
        }
        self.put_batch_internal(concepts).await
    }

    /// Internal write path for the mutation queue: identical to
    /// `put_batch` minus the reserved-prefix check (staging notes live
    /// under `/mutation-queue/` by design). The reserved-basename check
    /// (index.md/log.md/info.md) still applies — a queue path never
    /// collides with those, but the bypass must not weaken it.
    pub async fn put_batch_internal(&self, concepts: &[Concept]) -> Result<(), ConceptStoreError> {
        self.put_batch_with_revision(concepts, self.revision_op.clone(), None)
            .await
    }

    /// The single write-body: the batch put with the boundary's revision
    /// hook (spec §3.2 write-ahead — every record precedes its effect).
    /// `op_override` carries [`with_revision_op`]'s explicit tag;
    /// `summary_override` carries `restore`'s deterministic summary.
    async fn put_batch_with_revision(
        &self,
        concepts: &[Concept],
        op_override: Option<String>,
        summary_override: Option<String>,
    ) -> Result<(), ConceptStoreError> {
        if concepts.is_empty() {
            return Ok(());
        }
        // Provenance stamping + lifecycle validation happen BEFORE any
        // write, on clones (the incoming slice is never mutated):
        //   * the reserved-basename check stays pre-write;
        //   * `confirmed_at` is ALWAYS stamped (now) — a system field,
        //     the author's value never survives;
        //   * `source` is stamped only when the store is labeled (an
        //     unstamped store preserves the author's frontmatter; an
        //     EMPTY label is treated as unlabeled — legacy queue rows
        //     predate the source column and carry the '' default);
        //   * `validated()` is the write-time lifecycle guard — one bad
        //     value fails the whole batch before the first file lands,
        //     and before any revision row (guard-before-revision).
        let mut stamped = Vec::with_capacity(concepts.len());
        for concept in concepts {
            let basename = concept.source_path.rsplit('/').next().unwrap_or_default();
            if matches!(basename, "index.md" | "log.md" | "info.md") {
                return Err(ConceptStoreError::Reserved(concept.source_path.clone()));
            }
            let mut c = concept.clone();
            c.frontmatter.confirmed_at = Some(Utc::now().to_rfc3339());
            if let Some(src) = &self.provenance
                && !src.is_empty()
            {
                c.frontmatter.source = Some(src.clone());
            }
            c.frontmatter.validated()?;
            stamped.push(c);
        }
        let scope_id = self.scope.scope_id();
        for concept in &stamped {
            let markdown = concept.to_markdown()?;
            // Revision hook (write-ahead, §3.2 + §3.2a-5): 1. the
            // `/mutation-queue/` staging bypass records nothing
            // (queue-internal scaffolding, not user concepts); 2. op =
            // the explicit tag if set, else the registry row's existence
            // decides create/update; 3–4. payload per §3.2a-3 (create =
            // the created markdown, update/others = the RAW pre-mutation
            // file bytes), record BEFORE any effect — so a failing record
            // fails the put with no revision-less change (Review Focus
            // #5: no half-applied state).
            if !concept.source_path.starts_with("/mutation-queue/") {
                let (op, payload) = match &op_override {
                    Some(tag) if tag == "create" => (tag.clone(), markdown.as_bytes().to_vec()),
                    Some(tag) => match self
                        .repo
                        .read(&concept.source_path, &self.scope.repo_scope())
                        .await
                    {
                        // Explicit-tagged ops act on live concepts: the
                        // pre-mutation state is the payload. A vanished
                        // concept reads as empty pre-state for `restore`
                        // (undelete: no prior state, §3.2a-3) and as
                        // create-content for any other tag.
                        Ok(pre) => (tag.clone(), pre),
                        Err(FileRepoError::Io(io)) if io.kind() == std::io::ErrorKind::NotFound => {
                            let payload = if tag == "restore" {
                                Vec::new()
                            } else {
                                markdown.as_bytes().to_vec()
                            };
                            (tag.clone(), payload)
                        }
                        Err(other) => return Err(other.into()),
                    },
                    None => {
                        let existed: i64 = sqlx::query_scalar(
                            "SELECT EXISTS(SELECT 1 FROM scope_files
                                 WHERE scope = ? AND path = ?)",
                        )
                        .bind(&scope_id)
                        .bind(&concept.source_path)
                        .fetch_one(self.store.pool())
                        .await?;
                        if existed == 0 {
                            ("create".to_string(), markdown.as_bytes().to_vec())
                        } else {
                            match self
                                .repo
                                .read(&concept.source_path, &self.scope.repo_scope())
                                .await
                            {
                                Ok(pre) => ("update".to_string(), pre),
                                // Registry row but no file (a half-written
                                // state): nothing prior exists — create
                                // semantics.
                                Err(FileRepoError::Io(io))
                                    if io.kind() == std::io::ErrorKind::NotFound =>
                                {
                                    ("create".to_string(), markdown.as_bytes().to_vec())
                                }
                                Err(other) => return Err(other.into()),
                            }
                        }
                    }
                };
                let summary = match summary_override {
                    Some(ref s) => s.clone(),
                    None => op_summary(&op, None)?,
                };
                // Provenance in the row: a labeled store mirrors its label
                // (the stamped value: it overwrote the frontmatter on
                // disk); an EMPTY label records the unknown-provenance
                // convention (`''`, even though the concept's frontmatter
                // keeps the author value); an unlabeled store mirrors what
                // the concept carries — the author's declaration is the
                // disk truth.
                let rev_source = match &self.provenance {
                    Some(src) if !src.is_empty() => src.clone(),
                    Some(_) => String::new(),
                    None => concept.frontmatter.source.clone().unwrap_or_default(),
                };
                self.revisions
                    .record_as(&concept.source_path, &op, &rev_source, &summary, &payload)
                    .await?;
            }
            // The EXISTING effect steps: file → registry → index.
            self.repo
                .write(
                    &concept.source_path,
                    markdown.as_bytes(),
                    &self.scope.repo_scope(),
                )
                .await?;
            let now = Utc::now().to_rfc3339();
            let title = concept
                .frontmatter
                .title
                .clone()
                .unwrap_or_else(|| concept.source_path.clone());
            sqlx::query(
                "INSERT INTO scope_files (scope, path, title, concept_type, updated_at)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(scope, path) DO UPDATE SET
                   title = excluded.title,
                   concept_type = excluded.concept_type,
                   updated_at = excluded.updated_at",
            )
            .bind(&scope_id)
            .bind(&concept.source_path)
            .bind(&title)
            .bind(&concept.frontmatter.concept_type)
            .bind(&now)
            .execute(self.store.pool())
            .await?;
            self.index.add_async(concept).await?;
        }
        self.regen_index_md().await?;
        Ok(())
    }

    /// Restore-to-N (spec §3.2, the undo button's store action): decode
    /// snapshot N, parse it, and put it through the NORMAL stamped write
    /// path — `confirmed_at` re-stamps unconditionally, `source` per the
    /// provenance label rules — committing the result as a NEW revision
    /// (`op: restore`, summary `restored to revision {n}`); the restore
    /// row's payload is the pre-restore raw markdown, empty when it
    /// UNDELETES. Restoring a deleted chain's revision re-creates the
    /// concept at its original path (chains continue across eras, so a
    /// roll-back is itself reversible). Restored `status` is whatever the
    /// snapshot said — restore is literal; the separate set-active action
    /// revives.
    pub async fn restore(&self, path: &str, revision_no: u64) -> Result<(), ConceptStoreError> {
        if path.starts_with("/mutation-queue/") {
            return Err(ConceptStoreError::QueuePath(path.to_string()));
        }
        let snapshot = self.revisions.get(path, revision_no).await?;
        let markdown = String::from_utf8(snapshot).map_err(|_| {
            ConceptStoreError::Revisions(RevisionsError::Corrupted {
                detail: format!("revision {revision_no} of {path} is not UTF-8 markdown"),
            })
        })?;
        // A snapshot that no longer parses (a bad frontmatter value) fails
        // loudly here — the REST surface maps Parse to 400.
        let concept = Concept::parse(path, &markdown)?;
        let summary = op_summary("restore", Some(revision_no))?;
        self.put_batch_with_revision(
            std::slice::from_ref(&concept),
            Some("restore".to_string()),
            Some(summary),
        )
        .await
    }

    /// Regenerate the scope's `index.md` (v1 parity: the auto-
    /// maintained directory index). It is a RAW FileRepo payload —
    /// never a concept, never in the registry or search index — so it
    /// never appears in listings or graph scans.
    ///
    /// v1 structure: per-directory indexes with one bullet per concept
    /// (`[Title](relative-url) - description`) and a Subdirectories
    /// section (each subdir summarized: count, types, first titles).
    /// The registry rows carry title/type but not description, so the
    /// bullets use title + type (deterministic, no decrypt cost).
    async fn regen_index_md(&self) -> Result<(), ConceptStoreError> {
        let entries = self.list().await?;
        // Group entries by directory.
        let mut dirs: std::collections::BTreeMap<String, Vec<&ConceptEntry>> =
            std::collections::BTreeMap::new();
        for e in &entries {
            let dir = match e.path.rsplit_once('/') {
                Some((d, _)) => d.to_string(),
                None => String::new(),
            };
            dirs.entry(dir).or_default().push(e);
        }
        // The root index always exists (v1 parity), even when every
        // concept lives in a subdirectory.
        dirs.entry(String::new()).or_default();
        // Write one index.md per directory that has concepts.
        for (dir, ents) in &dirs {
            let mut lines = Vec::new();
            let name = if dir.is_empty() {
                "Knowledge Base".to_string()
            } else {
                dir.rsplit('/').next().unwrap_or(dir).to_string()
            };
            lines.push(format!("# {name}\n"));
            for e in ents {
                let basename = e.path.rsplit('/').next().unwrap_or(&e.path);
                lines.push(format!(
                    "* [{}]({}) [{}]",
                    e.title, basename, e.concept_type
                ));
            }
            // Subdirectories of this directory (immediate children).
            let prefix = if dir.is_empty() {
                "/".to_string()
            } else {
                format!("{dir}/")
            };
            let mut subdirs: std::collections::BTreeMap<String, Vec<&ConceptEntry>> =
                std::collections::BTreeMap::new();
            for (other_dir, other_ents) in &dirs {
                if let Some(rest) = other_dir.strip_prefix(&prefix)
                    && *other_dir != *dir
                    && !rest.is_empty()
                {
                    let child = rest.split('/').next().unwrap_or(rest);
                    subdirs
                        .entry(format!("{prefix}{child}"))
                        .or_default()
                        .extend(other_ents.iter().copied());
                }
            }
            if !subdirs.is_empty() {
                lines.push("\n## Subdirectories\n".to_string());
                for (sub, sub_ents) in &subdirs {
                    let types: Vec<String> = {
                        let mut t: Vec<String> = sub_ents
                            .iter()
                            .map(|e| e.concept_type.clone())
                            .filter(|t| !t.is_empty())
                            .collect();
                        t.sort();
                        t.dedup();
                        t
                    };
                    let type_list = if types.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", types.join(", "))
                    };
                    lines.push(format!(
                        "* [{sub}]({sub}/) — {} concept(s){type_list}",
                        sub_ents.len()
                    ));
                }
            }
            let content = lines.join("\n") + "\n";
            let path = format!("{dir}/index.md");
            self.repo
                .write(&path, content.as_bytes(), &self.scope.repo_scope())
                .await?;
        }
        Ok(())
    }

    /// Load and decrypt a concept.
    pub async fn get(&self, path: &str) -> Result<Concept, ConceptStoreError> {
        let bytes = self
            .repo
            .read(path, &self.scope.repo_scope())
            .await
            .map_err(|e| match e {
                FileRepoError::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
                    ConceptStoreError::NotFound(path.to_string())
                }
                other => other.into(),
            })?;
        let markdown =
            String::from_utf8(bytes).map_err(|_| ConceptStoreError::NotFound(path.to_string()))?;
        Ok(Concept::parse(path, &markdown)?)
    }

    /// Delete a concept: record the delete's revision row FIRST (with
    /// the pre-delete raw bytes — the write-ahead undo row, spec §3.2),
    /// then registry + index, then the file (a crash leaves an
    /// orphaned encrypted file — harmless, cleaned by maintenance;
    /// never a stale listing). Regenerates index.md. A concept that is
    /// already absent fails `NotFound` with no revision row and no
    /// side changes (the re-delete contract).
    ///
    /// Rejects the mutation queue's reserved `/mutation-queue/` prefix —
    /// only the queue's internal bypass may delete there (and staging-
    /// note deletes record no revision, symmetric with the put hook).
    pub async fn delete(&self, path: &str) -> Result<(), ConceptStoreError> {
        if path.starts_with("/mutation-queue/") {
            return Err(ConceptStoreError::QueuePath(path.to_string()));
        }
        self.delete_internal(path).await
    }

    /// Internal delete path for the mutation queue: the `delete` body
    /// minus the reserved-prefix check (the queue deletes its staging
    /// notes through this).
    pub async fn delete_internal(&self, path: &str) -> Result<(), ConceptStoreError> {
        // Write-ahead pre-state FIRST (§3.2a-5): a missing concept fails
        // NotFound with NO revision row and NO side changes (the
        // re-delete contract). The snapshot bytes ARE the delete row's
        // payload — captured before anything moves.
        let pre_state = match self.repo.read(path, &self.scope.repo_scope()).await {
            Ok(bytes) => bytes,
            Err(FileRepoError::Io(io)) if io.kind() == std::io::ErrorKind::NotFound => {
                return Err(ConceptStoreError::NotFound(path.to_string()));
            }
            Err(other) => return Err(other.into()),
        };
        // The delete row, write-ahead (§3.2): source is the boundary's
        // provenance label ('' when unlabeled — a delete's row records
        // the ACTOR, and this path has no system actor when the store
        // carries none). Staging notes bypass, symmetric with the put
        // hook (queue-internal scaffolding, not user concepts).
        if !path.starts_with("/mutation-queue/") {
            let rev_source = match &self.provenance {
                Some(src) if !src.is_empty() => src.clone(),
                _ => String::new(),
            };
            self.revisions
                .record_as(
                    path,
                    "delete",
                    &rev_source,
                    &op_summary("delete", None)?,
                    &pre_state,
                )
                .await?;
        }
        // The EXISTING steps: registry first, then index, then file (a
        // crash keeps the ordering invariants).
        let scope_id = self.scope.scope_id();
        sqlx::query("DELETE FROM scope_files WHERE scope = ? AND path = ?")
            .bind(&scope_id)
            .bind(path)
            .execute(self.store.pool())
            .await?;
        self.index.remove(path).await?;
        self.repo.delete(path, &self.scope.repo_scope()).await?;
        self.regen_index_md().await?;
        Ok(())
    }

    /// List a scope's concepts (registry order: path asc).
    pub async fn list(&self) -> Result<Vec<ConceptEntry>, ConceptStoreError> {
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT path, title, concept_type, updated_at FROM scope_files
             WHERE scope = ? ORDER BY path",
        )
        .bind(self.scope.scope_id())
        .fetch_all(self.store.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(path, title, concept_type, updated_at)| ConceptEntry {
                path,
                title,
                concept_type,
                updated_at,
            })
            .collect())
    }

    /// List a scope's concepts whose path starts with `prefix` (e.g. a
    /// book's `/<slug>/` chapter tree), registry order: path asc.
    pub async fn list_prefix(&self, prefix: &str) -> Result<Vec<ConceptEntry>, ConceptStoreError> {
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT path, title, concept_type, updated_at FROM scope_files
             WHERE scope = ? AND path LIKE ? ESCAPE '\\' ORDER BY path",
        )
        .bind(self.scope.scope_id())
        .bind(like_escape(prefix))
        .fetch_all(self.store.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|(path, title, concept_type, updated_at)| ConceptEntry {
                path,
                title,
                concept_type,
                updated_at,
            })
            .collect())
    }

    /// Search this scope (delegates to the encrypted index).
    pub async fn search(
        &self,
        query: &mycelium_core::search::SearchQuery,
    ) -> Result<Vec<mycelium_core::search::SearchResult>, ConceptStoreError> {
        Ok(self.index.search(query).await?)
    }
}

/// Escape a LIKE pattern's wildcards so `list_prefix` matches a literal
/// prefix (`%`/`_` in paths — e.g. slugs — must not act as wildcards),
/// then append the trailing `%` wildcard for the prefix match.
fn like_escape(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + 4);
    for c in prefix.chars() {
        if c == '%' || c == '_' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use mycelium_core::concept::Frontmatter;

    fn concept(path: &str, title: &str, body: &str) -> Concept {
        Concept::new(
            Frontmatter {
                concept_type: "Note".into(),
                title: Some(title.into()),
                ..Default::default()
            },
            body.into(),
            path.into(),
        )
    }

    #[tokio::test]
    async fn put_get_list_delete_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let user = Uuid::new_v4();
        let master = mycelium_crypto::generate_master_key();
        let cs = ConceptStore::for_user(&store, user, master);

        cs.put(&concept("/notes/a.md", "Alpha Note", "alpha body zebra"))
            .await
            .unwrap();
        cs.put(&concept("/notes/b.md", "Beta Note", "beta body"))
            .await
            .unwrap();

        // Get round-trips through encryption.
        let got = cs.get("/notes/a.md").await.unwrap();
        assert_eq!(got.frontmatter.title.as_deref(), Some("Alpha Note"));
        assert_eq!(got.body.trim(), "alpha body zebra");

        // List is sorted by path.
        let list = cs.list().await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].path, "/notes/a.md");
        assert_eq!(list[0].title, "Alpha Note");

        // Search finds it.
        let hits = cs
            .search(&mycelium_core::search::SearchQuery::new(vec![
                "zebra".into(),
            ]))
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);

        // Delete removes file + index + registry.
        cs.delete("/notes/a.md").await.unwrap();
        assert!(matches!(
            cs.get("/notes/a.md").await,
            Err(ConceptStoreError::NotFound(_))
        ));
        assert_eq!(cs.list().await.unwrap().len(), 1);
        let hits = cs
            .search(&mycelium_core::search::SearchQuery::new(vec![
                "zebra".into(),
            ]))
            .await
            .unwrap();
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn overwrite_updates_registry() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let user = Uuid::new_v4();
        let master = mycelium_crypto::generate_master_key();
        let cs = ConceptStore::for_user(&store, user, master);
        cs.put(&concept("/x.md", "V1", "one")).await.unwrap();
        cs.put(&concept("/x.md", "V2", "two")).await.unwrap();
        let list = cs.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].title, "V2");
        let got = cs.get("/x.md").await.unwrap();
        assert_eq!(got.frontmatter.title.as_deref(), Some("V2"));
    }

    #[tokio::test]
    async fn service_scope_is_isolated_from_user_scope() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let user = Uuid::new_v4();
        let master = mycelium_crypto::generate_master_key();
        let service = ServiceKey::from_bytes(&[5u8; 32]).unwrap();
        let user_cs = ConceptStore::for_user(&store, user, master);
        let service_cs =
            ConceptStore::for_service(&store, service, &dir.path().join("skills"), "skills");

        user_cs
            .put(&concept("/skills/private.md", "Private", "private skill"))
            .await
            .unwrap();
        service_cs
            .put(&concept("/skills/global.md", "Global", "global skill"))
            .await
            .unwrap();

        // Each scope lists only its own files.
        assert_eq!(user_cs.list().await.unwrap().len(), 1);
        assert_eq!(service_cs.list().await.unwrap().len(), 1);
        // Cross-scope get fails.
        assert!(service_cs.get("/skills/private.md").await.is_err());
        assert!(user_cs.get("/skills/global.md").await.is_err());
    }

    #[tokio::test]
    async fn service_namespaces_are_isolated() {
        // The skills shelf and the library catalogs share the service
        // key but use different registry namespaces — listings and
        // search must never mix.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let service = ServiceKey::from_bytes(&[6u8; 32]).unwrap();
        let skills = ConceptStore::for_service(
            &store,
            service.clone(),
            &dir.path().join("skills"),
            "skills",
        );
        let library = ConceptStore::for_service(
            &store,
            service.clone(),
            &dir.path().join("library"),
            "library",
        );

        skills
            .put(&concept("/deploy.md", "Deploy Skill", "deploy zebra"))
            .await
            .unwrap();
        library
            .put(&concept("/my-book/book.md", "My Book", "book zebra"))
            .await
            .unwrap();

        // Listings are per-namespace.
        let skill_list = skills.list().await.unwrap();
        assert_eq!(skill_list.len(), 1);
        assert_eq!(skill_list[0].path, "/deploy.md");
        let library_list = library.list().await.unwrap();
        assert_eq!(library_list.len(), 1);
        assert_eq!(library_list[0].path, "/my-book/book.md");

        // Search is per-namespace (both hit "zebra", each sees only its own).
        let q = mycelium_core::search::SearchQuery::new(vec!["zebra".into()]);
        assert_eq!(skills.search(&q).await.unwrap().len(), 1);
        assert_eq!(library.search(&q).await.unwrap().len(), 1);
        assert_eq!(
            skills.search(&q).await.unwrap()[0].concept_path,
            "/deploy.md"
        );
        assert_eq!(
            library.search(&q).await.unwrap()[0].concept_path,
            "/my-book/book.md"
        );

        // list_prefix returns only the book's chapter tree.
        library
            .put(&concept(
                "/my-book/ch-1-intro.md",
                "Chapter 1",
                "intro zebra",
            ))
            .await
            .unwrap();
        library
            .put(&concept("/other-book/book.md", "Other Book", "other zebra"))
            .await
            .unwrap();
        let chapters = library.list_prefix("/my-book/").await.unwrap();
        assert_eq!(chapters.len(), 2);
        assert!(chapters.iter().all(|c| c.path.starts_with("/my-book/")));

        // LIKE wildcards in the prefix are literal, not wildcards.
        let pct = library.list_prefix("/my-%/").await.unwrap();
        assert!(pct.is_empty());
    }

    #[tokio::test]
    async fn provenance_stamp_on_put_overwrites_spoofed_source_and_sets_confirmed_at() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let user = Uuid::new_v4();
        let master = mycelium_crypto::generate_master_key();
        let cs = ConceptStore::for_user(&store, user, master).with_provenance("mcp:test");
        let mut c = concept("/lifecycle/labeled.md", "Labeled", "labeled body");
        c.frontmatter.source = Some("llm-spoofed".into());
        c.frontmatter.confirmed_at = Some("2020-01-01T00:00:00Z".into());
        cs.put(&c).await.unwrap();
        let got = cs.get("/lifecycle/labeled.md").await.unwrap();
        assert_eq!(got.frontmatter.source.as_deref(), Some("mcp:test"));
        let confirmed = got
            .frontmatter
            .confirmed_at
            .as_deref()
            .expect("confirmed_at stamped");
        let t = chrono::DateTime::parse_from_rfc3339(confirmed).unwrap();
        assert!(
            t.timestamp() >= chrono::Utc::now().timestamp() - 300,
            "confirmed_at is now-ish, got {confirmed}"
        );
    }

    #[tokio::test]
    async fn no_label_put_preserves_author_source() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let user = Uuid::new_v4();
        let master = mycelium_crypto::generate_master_key();
        let cs = ConceptStore::for_user(&store, user, master);
        let mut c = concept("/lifecycle/unlabeled.md", "Unlabeled", "unlabeled body");
        c.frontmatter.source = Some("web:form".into());
        cs.put(&c).await.unwrap();
        let got = cs.get("/lifecycle/unlabeled.md").await.unwrap();
        assert_eq!(got.frontmatter.source.as_deref(), Some("web:form"));
        let confirmed = got
            .frontmatter
            .confirmed_at
            .as_deref()
            .expect("confirmed_at is still stamped");
        assert!(
            chrono::DateTime::parse_from_rfc3339(confirmed).is_ok(),
            "confirmed_at is RFC 3339, got {confirmed}"
        );
    }

    #[tokio::test]
    async fn put_rejects_invalid_lifecycle_values() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let user = Uuid::new_v4();
        let master = mycelium_crypto::generate_master_key();
        let cs = ConceptStore::for_user(&store, user, master);

        let mut bad = concept("/lifecycle/bad.md", "Bad", "bad body");
        bad.frontmatter.status = Some("archive".into());
        let err = cs.put(&bad).await.unwrap_err();
        assert!(
            matches!(
                err,
                ConceptStoreError::Parse(
                    mycelium_core::ConceptError::InvalidLifecycleStatus { .. }
                )
            ),
            "expected the lifecycle guard, got {err}"
        );
        // Rejected before any write: nothing landed in the registry.
        assert!(cs.list().await.unwrap().is_empty());

        let mut good = concept("/lifecycle/good.md", "Good", "good body");
        good.frontmatter.status = Some("retired".into());
        cs.put(&good).await.unwrap();
        let got = cs.get("/lifecycle/good.md").await.unwrap();
        assert_eq!(got.frontmatter.status.as_deref(), Some("retired"));
    }

    #[tokio::test]
    async fn empty_label_put_is_unlabeled() {
        // Legacy queue rows predate the source column and carry '' — an
        // empty label is treated as unlabeled: the author's source stays,
        // `confirmed_at` is still stamped (mirror of no_label_put_…;
        // the real call site passes `&item.source` unconditionally).
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        let user = Uuid::new_v4();
        let master = mycelium_crypto::generate_master_key();
        let cs = ConceptStore::for_user(&store, user, master).with_provenance("");
        let mut c = concept("/lifecycle/empty.md", "Empty Label", "empty body");
        c.frontmatter.source = Some("web:form".into());
        cs.put(&c).await.unwrap();
        let got = cs.get("/lifecycle/empty.md").await.unwrap();
        assert_eq!(got.frontmatter.source.as_deref(), Some("web:form"));
        let confirmed = got
            .frontmatter
            .confirmed_at
            .as_deref()
            .expect("confirmed_at is still stamped");
        assert!(
            chrono::DateTime::parse_from_rfc3339(confirmed).is_ok(),
            "confirmed_at is RFC 3339, got {confirmed}"
        );
    }
}
