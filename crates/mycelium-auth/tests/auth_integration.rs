//! End-to-end auth integration test:
//! bootstrap → login → session → RBAC extractors grant/reject.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use mycelium_auth::bootstrap::bootstrap_admin;
use mycelium_auth::rbac::{RequireAdmin, Role, SessionUser};
use mycelium_auth::session::SessionManager;
use mycelium_auth::users::UserStore;
use mycelium_store::Store;
use uuid::Uuid;

fn empty_parts() -> Parts {
    // Parts has no Default; construct via a dummy request.
    use axum::http::Request;
    let request: Request<()> = Request::get("/").body(()).unwrap();
    let (parts, _) = request.into_parts();
    parts
}

#[tokio::test]
async fn bootstrap_login_session_rbac_flow() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).await.unwrap();

    // 1. Bootstrap creates the first admin (with must_change_password).
    let admin = bootstrap_admin(&store)
        .await
        .unwrap()
        .expect("first run bootstraps");
    assert_eq!(admin.role, Role::Admin);
    assert!(admin.must_change_password);

    // 2. Second bootstrap call is a no-op.
    assert!(bootstrap_admin(&store).await.unwrap().is_none());

    // 3. Read the generated password and log in.
    let password = std::fs::read_to_string(dir.path().join("config/initial-admin-password"))
        .unwrap()
        .trim()
        .to_string();
    let users = UserStore::new(store.pool().clone());
    let auth = users.authenticate_local("admin", &password).await.unwrap();
    assert_eq!(auth.record.id, admin.id);

    // 4. Create a session.
    let sessions = SessionManager::new(store.pool().clone());
    let session = sessions.create(auth.record.id).await.unwrap();
    let got = sessions.get(session.id).await.unwrap();
    assert_eq!(got.user_id, auth.record.id);

    // 5. RBAC extractors: admin passes RequireAdmin.
    let admin_user = SessionUser {
        user_id: auth.record.id,
        username: auth.record.username.clone(),
        role: Role::Admin,
    };
    let mut parts = empty_parts();
    parts.extensions.insert(admin_user.clone());
    assert!(
        RequireAdmin::from_request_parts(&mut parts, &())
            .await
            .is_ok()
    );

    // 6. A regular user is rejected by RequireAdmin but passes SessionUser.
    let regular = UserRecord::new_user();
    let mut parts = empty_parts();
    parts.extensions.insert(regular.clone());
    assert!(
        RequireAdmin::from_request_parts(&mut parts, &())
            .await
            .is_err()
    );
    assert!(
        SessionUser::from_request_parts(&mut parts, &())
            .await
            .is_ok()
    );

    // 7. No extension → both extractors reject (401).
    let mut parts = empty_parts();
    assert!(
        SessionUser::from_request_parts(&mut parts, &())
            .await
            .is_err()
    );
    assert!(
        RequireAdmin::from_request_parts(&mut parts, &())
            .await
            .is_err()
    );

    // 8. Logout deletes the session.
    sessions.delete(session.id).await.unwrap();
    assert!(sessions.get(session.id).await.is_err());
}

/// Helper: a plain user SessionUser for extractor tests.
struct UserRecord;

impl UserRecord {
    fn new_user() -> SessionUser {
        SessionUser {
            user_id: Uuid::new_v4(),
            username: "regular".into(),
            role: Role::User,
        }
    }
}
