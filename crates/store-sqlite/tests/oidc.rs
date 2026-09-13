//! Signing in through a provider, against a real store: the first visit asks for a username
//! and creates the account, the second just signs in, and a banned account is refused.

use notespace_core::model::UserState;
use notespace_core::oidc::{self, Finish, FinishRejected, ProviderKind, RemoteIdentity, SignIn};
use notespace_core::session::{SessionPolicy, SessionToken, TOKEN_BYTES};
use notespace_core::store::Store;
use notespace_store_sqlite::SqliteStore;

const MIGRATIONS: [&str; 10] = [
    include_str!("../../../migrations/0001_init.sql"),
    include_str!("../../../migrations/0002_thread_public_id.sql"),
    include_str!("../../../migrations/0003_space_paths_and_names.sql"),
    include_str!("../../../migrations/0004_post_public_id.sql"),
    include_str!("../../../migrations/0005_session.sql"),
    include_str!("../../../migrations/0006_login_attempt.sql"),
    include_str!("../../../migrations/0007_user_password.sql"),
    include_str!("../../../migrations/0008_moderation.sql"),
    include_str!("../../../migrations/0009_email_and_spaces.sql"),
    include_str!("../../../migrations/0010_external_identity.sql"),
];

const NOW: i64 = 1_800_000_000_000;

fn identity(subject: &str, email: Option<&str>, verified: bool) -> RemoteIdentity {
    RemoteIdentity {
        provider: ProviderKind::Google,
        subject: subject.into(),
        email: email.map(str::to_string),
        email_verified: verified,
        suggested_name: Some("Some One".into()),
    }
}

fn token(seed: u8) -> SessionToken {
    SessionToken::from_bytes([seed; TOKEN_BYTES])
}

#[tokio::test]
async fn first_visit_needs_a_name_then_creates_a_passwordless_account_with_a_verified_email() {
    let store = SqliteStore::in_memory(&MIGRATIONS).unwrap();
    let policy = SessionPolicy::default();
    let id = identity("g-1", Some("Some.One@Example.com"), true);

    assert!(matches!(
        oidc::sign_in(&store, &id, token(1), &policy, NOW)
            .await
            .unwrap(),
        SignIn::NeedsAccount
    ));
    assert_eq!(id.suggested_username(), "some-one");

    let user = match oidc::finish(&store, &id, "some-one", token(2), &policy, NOW)
        .await
        .unwrap()
    {
        Finish::Created { user, session } => {
            assert!(store
                .lookup_session(&session.token_hash, NOW)
                .await
                .unwrap()
                .is_some());
            user
        }
        _ => panic!("expected Created"),
    };
    assert_eq!(user.name, "some-one");
    let account = store.account(user.id).await.unwrap().unwrap();
    assert!(!account.has_password);
    assert_eq!(account.email.as_deref(), Some("some.one@example.com"));
    assert!(account.email_is_verified(), "the provider vouched for it");
    assert_eq!(
        store.user_identities(user.id).await.unwrap(),
        vec!["google".to_string()]
    );

    // Second visit: straight in.
    match oidc::sign_in(&store, &id, token(3), &policy, NOW + 1)
        .await
        .unwrap()
    {
        SignIn::Done { user: again, .. } => assert_eq!(again.id, user.id),
        _ => panic!("a linked identity did not sign in"),
    }
}

#[tokio::test]
async fn an_unverified_address_is_kept_but_not_trusted() {
    let store = SqliteStore::in_memory(&MIGRATIONS).unwrap();
    let id = identity("g-2", Some("maybe@example.com"), false);
    let Finish::Created { user, .. } = oidc::finish(
        &store,
        &id,
        "maybe",
        token(4),
        &SessionPolicy::default(),
        NOW,
    )
    .await
    .unwrap() else {
        panic!()
    };
    let account = store.account(user.id).await.unwrap().unwrap();
    assert_eq!(account.email.as_deref(), Some("maybe@example.com"));
    assert!(!account.email_is_verified());
}

#[tokio::test]
async fn a_taken_name_a_bad_name_and_a_raced_identity_are_refused() {
    let store = SqliteStore::in_memory(&MIGRATIONS).unwrap();
    let policy = SessionPolicy::default();
    store.create_user("incumbent", NOW, None).await.unwrap();
    let id = identity("g-3", None, false);
    assert!(matches!(
        oidc::finish(&store, &id, "incumbent", token(5), &policy, NOW)
            .await
            .unwrap(),
        Finish::Rejected(FinishRejected::Taken)
    ));
    assert!(matches!(
        oidc::finish(&store, &id, "admin", token(6), &policy, NOW)
            .await
            .unwrap(),
        Finish::Rejected(FinishRejected::BadName(_))
    ));
    assert!(matches!(
        oidc::finish(&store, &id, "fresh-name", token(7), &policy, NOW)
            .await
            .unwrap(),
        Finish::Created { .. }
    ));
    // The same identity again, from a stale pending cookie: refused, not a second account.
    assert!(matches!(
        oidc::finish(&store, &id, "another-name", token(8), &policy, NOW)
            .await
            .unwrap(),
        Finish::Rejected(FinishRejected::Taken)
    ));
    assert!(store.user_by_name("another-name").await.unwrap().is_none());
}

#[tokio::test]
async fn a_banned_account_cannot_sign_in_through_its_provider() {
    let store = SqliteStore::in_memory(&MIGRATIONS).unwrap();
    let policy = SessionPolicy::default();
    let id = identity("g-4", None, false);
    let Finish::Created { user, .. } =
        oidc::finish(&store, &id, "soon-banned", token(9), &policy, NOW)
            .await
            .unwrap()
    else {
        panic!()
    };
    store
        .set_user_state(user.id, UserState::Banned)
        .await
        .unwrap();
    assert!(matches!(
        oidc::sign_in(&store, &id, token(10), &policy, NOW)
            .await
            .unwrap(),
        SignIn::Refused
    ));
}
