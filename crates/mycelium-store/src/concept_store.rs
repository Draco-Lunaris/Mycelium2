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
    },
}

impl ConceptScope {
    fn scope_id(&self) -> String {
        match self {
            ConceptScope::User { user_id, .. } => format!("user:{user_id}"),
            ConceptScope::Service { .. } => "global".to_string(),
        }
    }

    fn repo_scope(&self) -> Scope {
        match self {
            ConceptScope::User { master_key, .. } => Scope::User(master_key.clone()),
            ConceptScope::Service { service_key } => Scope::Service(service_key.clone()),
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

    /// Open the concept store for a service-key scope (global skills shelf,
    /// shared library stacks).
    pub fn for_service(store: &'a Store, service_key: ServiceKey, dir: &std::path::Path) -> Self {
        let repo = FileRepo::new(dir);
        let index = EncryptedIndex::for_service(store.pool().clone(), &service_key);
        Self {
            store,
            scope: ConceptScope::Service { service_key },
            repo,
            index,
        }
    }

    /// Store a concept: encrypt the file, register it, then index it.
    /// Ordering: file → registry → index. The registry is the source of
    /// truth for listing; the index lags it, never leads. A crash between
    /// steps leaves an orphaned encrypted file or a listed-but-unsearchable
    /// concept (visible, self-heals on next put) — never a search ghost.
    pub async fn put(&self, concept: &Concept) -> Result<(), ConceptStoreError> {
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
    /// maintenance; never a stale listing).
    pub async fn delete(&self, path: &str) -> Result<(), ConceptStoreError> {
        let scope_id = self.scope.scope_id();
        sqlx::query("DELETE FROM scope_files WHERE scope = ? AND path = ?")
            .bind(&scope_id)
            .bind(path)
            .execute(self.store.pool())
            .await?;
        self.index.remove(path).await?;
        self.repo.delete(path, &self.scope.repo_scope()).await?;
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

    /// Search this scope (delegates to the encrypted index).
    pub async fn search(
        &self,
        query: &mycelium_core::search::SearchQuery,
    ) -> Result<Vec<mycelium_core::search::SearchResult>, ConceptStoreError> {
        Ok(self.index.search(query).await?)
    }
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
                description: None,
                resource: None,
                tags: vec![],
                timestamp: None,
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
        let service_cs = ConceptStore::for_service(&store, service, &dir.path().join("skills"));

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
}
