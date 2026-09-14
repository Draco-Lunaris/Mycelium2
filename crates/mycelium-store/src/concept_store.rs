//! Concept store: the coordinated facade over FileRepo + EncryptedIndex +
//! the scope_files registry — one place that keeps files, index, and
//! listing metadata in sync (the Phase 3 review's "atomic lifecycle" gap).

use chrono::Utc;
use mycelium_core::concept::Concept;
use mycelium_crypto::keys::{MasterKey, ServiceKey};
use uuid::Uuid;

use crate::Store;
use crate::file_repo::{FileRepo, FileRepoError, Scope};
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
    #[error("concept not found: {0}")]
    NotFound(String),
    #[error("{0} is a reserved filename (index.md, log.md, info.md) — system-maintained")]
    Reserved(String),
}

/// A listed concept (from the registry, no decryption needed).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
}

impl<'a> ConceptStore<'a> {
    /// The scope's registry id (`user:<uuid>` / `global:<ns>`) — used by
    /// cache fingerprints and trace sinks.
    pub fn scope_id(&self) -> String {
        self.scope.scope_id()
    }

    /// Open the concept store for a user's private bundle.
    pub fn for_user(store: &'a Store, user_id: Uuid, master_key: MasterKey) -> Self {
        let repo = FileRepo::new(store.user_dir(user_id));
        let index =
            EncryptedIndex::for_user(store.pool().clone(), &user_id.to_string(), &master_key);
        Self {
            store,
            scope: ConceptScope::User {
                user_id,
                master_key,
            },
            repo,
            index,
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
        Self {
            store,
            scope: ConceptScope::Service {
                service_key,
                namespace: namespace.to_string(),
            },
            repo,
            index,
        }
    }

    /// Store a concept: encrypt the file, register it, index it, then
    /// regenerate the scope's index.md. Ordering: file → registry →
    /// index. The registry is the source of truth for listing; the
    /// index lags it, never leads. A crash between steps leaves an
    /// orphaned encrypted file or a listed-but-unsearchable concept
    /// (visible, self-heals on next put) — never a search ghost.
    ///
    /// Reserved filenames (`index.md`, `log.md`, `info.md`) are
    /// rejected — they are system-maintained (index.md is regenerated
    /// on every write) and a concept there would be silently clobbered.
    pub async fn put(&self, concept: &Concept) -> Result<(), ConceptStoreError> {
        self.put_batch(std::slice::from_ref(concept)).await
    }

    /// Batched put: writes many concepts with ONE index.md regeneration
    /// at the end (put() regenerates per concept — O(n²) on a
    /// 300-chapter catalog ingest). Same ordering guarantees as put():
    /// file → registry → index per concept, then the single regen.
    pub async fn put_batch(&self, concepts: &[Concept]) -> Result<(), ConceptStoreError> {
        if concepts.is_empty() {
            return Ok(());
        }
        for concept in concepts {
            let basename = concept.source_path.rsplit('/').next().unwrap_or_default();
            if matches!(basename, "index.md" | "log.md" | "info.md") {
                return Err(ConceptStoreError::Reserved(concept.source_path.clone()));
            }
            let markdown = concept.to_markdown()?;
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
            .bind(self.scope.scope_id())
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

    /// Delete a concept: registry + index first, then the file (a crash
    /// leaves an orphaned encrypted file — harmless, cleaned by
    /// maintenance; never a stale listing). Regenerates index.md.
    pub async fn delete(&self, path: &str) -> Result<(), ConceptStoreError> {
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
}
