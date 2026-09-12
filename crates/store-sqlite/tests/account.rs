//! Account self-service against a real store: signup with an address, verification, password
//! reset, and password change. The mailer records instead of sending, so the tests read the
//! link out of the mail the way a person would.

use std::cell::RefCell;

use notespace_core::account::{
    self, ChangeOutcome, ConfirmOutcome, Delivery, PasswordChange, RecoveryConfig, RequestOutcome,
    ResetCompletion, ResetOutcome, ResetRequest,
};
use notespace_core::email::{
    EmailToken, Links, MailError, Mailer, Message, TOKEN_BYTES as EMAIL_TOKEN_BYTES,
};
use notespace_core::login::{self, Attempt, LoginConfig};
use notespace_core::password::{Params, Pepper, PepperSet, Scheme};
use notespace_core::ratelimit::Limit;
use notespace_core::register::{self, Outcome as SignupOutcome, RegisterConfig, Signup};
use notespace_core::session::{SessionPolicy, SessionToken, TOKEN_BYTES};
use notespace_core::store::Store;
use notespace_store_sqlite::SqliteStore;

const MIGRATIONS: [&str; 9] = [
    include_str!("../../../migrations/0001_init.sql"),
    include_str!("../../../migrations/0002_thread_public_id.sql"),
    include_str!("../../../migrations/0003_space_paths_and_names.sql"),
    include_str!("../../../migrations/0004_post_public_id.sql"),
    include_str!("../../../migrations/0005_session.sql"),
    include_str!("../../../migrations/0006_login_attempt.sql"),
    include_str!("../../../migrations/0007_user_password.sql"),
    include_str!("../../../migrations/0008_moderation.sql"),
    include_str!("../../../migrations/0009_email_and_spaces.sql"),
];

const NOW: i64 = 1_800_000_000_000;
const PW: &str = "correct horse battery staple";
const NEW_PW: &str = "a different long password";
const FAST: Scheme = Scheme::Server(Params {
    m_kib: 64,
    t: 1,
    p: 1,
});
const LINKS: Links<'static> = Links {
    base_url: "https://forum.example",
    site_name: "forum",
};

/// Records every message; `fail` makes sends fail, to check that flows survive it.
#[derive(Default)]
struct Outbox {
    sent: RefCell<Vec<Message>>,
    fail: bool,
}

#[async_trait::async_trait(?Send)]
impl Mailer for Outbox {
    async fn send(&self, message: &Message) -> Result<(), MailError> {
        if self.fail {
            return Err(MailError::Transport("down".into()));
        }
        self.sent.borrow_mut().push(message.clone());
        Ok(())
    }
}

impl Outbox {
    fn last(&self) -> Message {
        self.sent.borrow().last().cloned().expect("a mail was sent")
    }

    /// The token out of the last mail's link, as a person following it would supply it.
    fn last_token(&self) -> EmailToken {
        let text = self.last().text;
        let start = text.find("token=").expect("a link") + "token=".len();
        let end = text[start..]
            .find(|c: char| !c.is_ascii_hexdigit())
            .map(|i| start + i)
            .unwrap_or(text.len());
        EmailToken::parse(&text[start..end]).expect("a well-formed token")
    }
}

fn peppers() -> PepperSet {
    PepperSet::single(0, Pepper::new(&[9u8; 32]).unwrap())
}

fn register_config(require_email: bool) -> RegisterConfig {
    RegisterConfig {
        scheme: FAST,
        peppers: peppers(),
        sessions: SessionPolicy::default(),
        per_client: Limit {
            max: 100,
            window_ms: 60_000,
        },
        require_email,
    }
}

fn recovery_config() -> RecoveryConfig {
    RecoveryConfig {
        scheme: FAST,
        peppers: peppers(),
        per_client: Limit {
            max: 100,
            window_ms: 60_000,
        },
        per_address: Limit {
            max: 3,
            window_ms: 60_000,
        },
    }
}

fn login_config() -> LoginConfig {
    LoginConfig {
        scheme: FAST,
        dummy_hash: LoginConfig::dummy_hash_for(FAST, &peppers()),
        peppers: peppers(),
        sessions: SessionPolicy::default(),
        per_identity: Limit {
            max: 100,
            window_ms: 60_000,
        },
        per_client: Limit {
            max: 100,
            window_ms: 60_000,
        },
    }
}

fn signup<'a>(name: &'a str, email: &'a str, seed: u8) -> Signup<'a> {
    Signup {
        username: name,
        password: PW,
        email,
        client: "203.0.113.9",
        token: SessionToken::from_bytes([seed; TOKEN_BYTES]),
        salt: "c29tZXNhbHR2YWx1ZTE",
        verify_token: EmailToken::from_bytes([seed; EMAIL_TOKEN_BYTES]),
        now: NOW,
    }
}

fn fresh_token(seed: u8) -> EmailToken {
    EmailToken::from_bytes([seed; EMAIL_TOKEN_BYTES])
}

async fn can_log_in(store: &SqliteStore, name: &str, pw: &str, seed: u8) -> bool {
    let out = login::attempt(
        store,
        &login_config(),
        Attempt {
            username: name,
            password: pw,
            client: "203.0.113.10",
            token: SessionToken::from_bytes([seed; TOKEN_BYTES]),
            now: NOW,
        },
    )
    .await
    .unwrap();
    matches!(out, login::Outcome::Success { .. })
}

fn store() -> SqliteStore {
    SqliteStore::in_memory(&MIGRATIONS).expect("migrations apply")
}

#[tokio::test]
async fn signup_with_an_address_mails_a_link_that_verifies_it() {
    let store = store();
    let outbox = Outbox::default();
    let user = match register::signup(
        &store,
        &outbox,
        &LINKS,
        &register_config(false),
        signup("alice", "Alice@Example.com", 1),
    )
    .await
    .unwrap()
    {
        SignupOutcome::Created {
            user, verification, ..
        } => {
            assert_eq!(verification, Some(Delivery::Sent));
            user
        }
        _ => panic!("expected Created"),
    };
    let mail = outbox.last();
    assert_eq!(
        mail.to.as_str(),
        "alice@example.com",
        "normalised before sending"
    );
    assert!(mail.text.contains("https://forum.example/verify?token="));

    let account = store.account(user.id).await.unwrap().unwrap();
    assert_eq!(account.email.as_deref(), Some("alice@example.com"));
    assert!(
        !account.email_is_verified(),
        "verified without following the link"
    );

    let token = outbox.last_token();
    assert_eq!(
        account::confirm_email(&store, &token, NOW + 1000)
            .await
            .unwrap(),
        ConfirmOutcome::Verified { user_id: user.id }
    );
    assert!(store
        .account(user.id)
        .await
        .unwrap()
        .unwrap()
        .email_is_verified());
    // The link is spent.
    assert_eq!(
        account::confirm_email(&store, &token, NOW + 2000)
            .await
            .unwrap(),
        ConfirmOutcome::Invalid
    );
}

#[tokio::test]
async fn a_signup_without_an_address_sends_nothing_unless_one_is_required() {
    let store = store();
    let outbox = Outbox::default();
    match register::signup(
        &store,
        &outbox,
        &LINKS,
        &register_config(false),
        signup("bob", "", 2),
    )
    .await
    .unwrap()
    {
        SignupOutcome::Created { verification, .. } => assert_eq!(verification, None),
        _ => panic!("expected Created"),
    }
    assert!(outbox.sent.borrow().is_empty());
    match register::signup(
        &store,
        &outbox,
        &LINKS,
        &register_config(true),
        signup("carol", "", 3),
    )
    .await
    .unwrap()
    {
        SignupOutcome::Rejected(register::Rejected::BadEmail(_)) => {}
        _ => panic!("an address was required and not supplied"),
    }
    assert!(store.user_by_name("carol").await.unwrap().is_none());
}

/// The account is the thing; the mail is a courtesy.
#[tokio::test]
async fn a_failed_send_does_not_fail_the_signup() {
    let store = store();
    let outbox = Outbox {
        fail: true,
        ..Default::default()
    };
    match register::signup(
        &store,
        &outbox,
        &LINKS,
        &register_config(false),
        signup("dave", "dave@example.com", 4),
    )
    .await
    .unwrap()
    {
        SignupOutcome::Created { verification, .. } => {
            assert!(matches!(verification, Some(Delivery::Failed(_))));
        }
        _ => panic!("expected Created"),
    }
    assert!(can_log_in(&store, "dave", PW, 40).await);
}

#[tokio::test]
async fn a_verification_link_expires_and_dies_with_an_address_change() {
    let store = store();
    let outbox = Outbox::default();
    let user = match register::signup(
        &store,
        &outbox,
        &LINKS,
        &register_config(false),
        signup("erin", "erin@example.com", 5),
    )
    .await
    .unwrap()
    {
        SignupOutcome::Created { user, .. } => user,
        _ => panic!(),
    };
    let first = outbox.last_token();
    let day = 24 * 60 * 60 * 1000;
    assert_eq!(
        account::confirm_email(&store, &first, NOW + day)
            .await
            .unwrap(),
        ConfirmOutcome::Invalid,
        "a day-old verify link still worked"
    );
    // A new address retires the old link and issues a new one.
    let delivery = account::change_email(
        &store,
        &outbox,
        &LINKS,
        &user,
        "erin2@example.com",
        fresh_token(50),
        NOW + 10,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(delivery, Delivery::Sent);
    assert_eq!(
        account::confirm_email(&store, &first, NOW + 20)
            .await
            .unwrap(),
        ConfirmOutcome::Invalid,
        "the old link survived an address change"
    );
    let second = outbox.last_token();
    assert_eq!(
        account::confirm_email(&store, &second, NOW + 30)
            .await
            .unwrap(),
        ConfirmOutcome::Verified { user_id: user.id }
    );
    let account = store.account(user.id).await.unwrap().unwrap();
    assert_eq!(account.email.as_deref(), Some("erin2@example.com"));
    assert!(account.email_is_verified());
}

#[tokio::test]
async fn a_link_sent_to_one_address_cannot_verify_another() {
    let store = store();
    let outbox = Outbox::default();
    let user = match register::signup(
        &store,
        &outbox,
        &LINKS,
        &register_config(false),
        signup("frank", "frank@example.com", 6),
    )
    .await
    .unwrap()
    {
        SignupOutcome::Created { user, .. } => user,
        _ => panic!(),
    };
    let token = outbox.last_token();
    // Address changed underneath the link, by a path that did not issue a new token.
    store
        .set_email(user.id, Some("other@example.com"))
        .await
        .unwrap();
    assert_eq!(
        account::confirm_email(&store, &token, NOW + 1)
            .await
            .unwrap(),
        ConfirmOutcome::Stale
    );
    assert!(!store
        .account(user.id)
        .await
        .unwrap()
        .unwrap()
        .email_is_verified());
}

#[tokio::test]
async fn the_second_account_to_prove_an_address_is_refused() {
    let store = store();
    let outbox = Outbox::default();
    for (name, seed) in [("gina", 7), ("hal", 8)] {
        register::signup(
            &store,
            &outbox,
            &LINKS,
            &register_config(false),
            signup(name, "shared@example.com", seed),
        )
        .await
        .unwrap();
        let token = outbox.last_token();
        let outcome = account::confirm_email(&store, &token, NOW + 1)
            .await
            .unwrap();
        match name {
            "gina" => assert!(matches!(outcome, ConfirmOutcome::Verified { .. })),
            _ => assert_eq!(outcome, ConfirmOutcome::Claimed),
        }
    }
    let owner = store
        .user_by_verified_email("shared@example.com")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(owner.name, "gina");
}

async fn verified_account(
    store: &SqliteStore,
    outbox: &Outbox,
    name: &str,
    email: &str,
    seed: u8,
) -> notespace_core::model::User {
    let user = match register::signup(
        store,
        outbox,
        &LINKS,
        &register_config(false),
        signup(name, email, seed),
    )
    .await
    .unwrap()
    {
        SignupOutcome::Created { user, .. } => user,
        _ => panic!(),
    };
    let token = outbox.last_token();
    assert!(matches!(
        account::confirm_email(store, &token, NOW).await.unwrap(),
        ConfirmOutcome::Verified { .. }
    ));
    user
}

#[tokio::test]
async fn a_reset_link_changes_the_password_and_ends_every_session() {
    let store = store();
    let outbox = Outbox::default();
    let user = verified_account(&store, &outbox, "ivy", "ivy@example.com", 9).await;
    assert!(can_log_in(&store, "ivy", PW, 90).await);
    let sessions_before = store.delete_user_sessions(user.id).await.unwrap();
    assert!(sessions_before >= 1, "no sessions to end");
    assert!(can_log_in(&store, "ivy", PW, 91).await);

    match account::request_reset(
        &store,
        &outbox,
        &LINKS,
        &recovery_config(),
        ResetRequest {
            email: "IVY@example.com",
            client: "203.0.113.11",
            token: fresh_token(60),
            now: NOW,
        },
    )
    .await
    .unwrap()
    {
        RequestOutcome::Accepted { delivery } => assert_eq!(delivery, Some(Delivery::Sent)),
        _ => panic!("expected Accepted"),
    }
    let mail = outbox.last();
    assert!(mail.text.contains("https://forum.example/reset?token="));
    let token = outbox.last_token();

    // A short password does not spend the link.
    match account::complete_reset(
        &store,
        &outbox,
        &LINKS,
        &recovery_config(),
        ResetCompletion {
            token: &token,
            new_password: "short",
            salt: "c29tZXNhbHR2YWx1ZTE",
            now: NOW + 1,
        },
    )
    .await
    .unwrap()
    {
        ResetOutcome::ShortPassword { .. } => {}
        _ => panic!("accepted a short password"),
    }
    match account::complete_reset(
        &store,
        &outbox,
        &LINKS,
        &recovery_config(),
        ResetCompletion {
            token: &token,
            new_password: NEW_PW,
            salt: "c29tZXNhbHR2YWx1ZTE",
            now: NOW + 2,
        },
    )
    .await
    .unwrap()
    {
        ResetOutcome::Done { user: u } => assert_eq!(u.id, user.id),
        _ => panic!("expected Done"),
    }
    assert!(
        !can_log_in(&store, "ivy", PW, 92).await,
        "the old password still works"
    );
    assert!(
        can_log_in(&store, "ivy", NEW_PW, 93).await,
        "the new password does not work"
    );
    // Every session from before the reset is gone: only the login just above remains.
    assert_eq!(store.delete_user_sessions(user.id).await.unwrap(), 1);
    // The link is spent, and the owner was told.
    match account::complete_reset(
        &store,
        &outbox,
        &LINKS,
        &recovery_config(),
        ResetCompletion {
            token: &token,
            new_password: NEW_PW,
            salt: "c29tZXNhbHR2YWx1ZTE",
            now: NOW + 3,
        },
    )
    .await
    .unwrap()
    {
        ResetOutcome::Invalid => {}
        _ => panic!("a spent reset link worked again"),
    }
    assert!(outbox.last().subject.contains("changed"));
}

/// The address is not an oracle: unknown, unverified and known addresses all get "accepted".
#[tokio::test]
async fn a_reset_request_says_the_same_thing_whatever_the_address() {
    let store = store();
    let outbox = Outbox::default();
    // Known but unverified.
    register::signup(
        &store,
        &outbox,
        &LINKS,
        &register_config(false),
        signup("jo", "jo@example.com", 10),
    )
    .await
    .unwrap();
    let before = outbox.sent.borrow().len();
    for addr in ["nobody@example.com", "jo@example.com"] {
        match account::request_reset(
            &store,
            &outbox,
            &LINKS,
            &recovery_config(),
            ResetRequest {
                email: addr,
                client: "203.0.113.12",
                token: fresh_token(70),
                now: NOW,
            },
        )
        .await
        .unwrap()
        {
            RequestOutcome::Accepted { delivery } => assert_eq!(delivery, None, "{addr}"),
            _ => panic!("{addr}: not accepted"),
        }
    }
    assert_eq!(
        outbox.sent.borrow().len(),
        before,
        "mail went to an unverified or unknown address"
    );
    match account::request_reset(
        &store,
        &outbox,
        &LINKS,
        &recovery_config(),
        ResetRequest {
            email: "not an address",
            client: "203.0.113.12",
            token: fresh_token(71),
            now: NOW,
        },
    )
    .await
    .unwrap()
    {
        RequestOutcome::BadAddress(_) => {}
        _ => panic!("a malformed address was accepted"),
    }
}

#[tokio::test]
async fn reset_requests_are_limited_per_address() {
    let store = store();
    let outbox = Outbox::default();
    verified_account(&store, &outbox, "kim", "kim@example.com", 11).await;
    let cfg = recovery_config();
    for i in 0..cfg.per_address.max {
        match account::request_reset(
            &store,
            &outbox,
            &LINKS,
            &cfg,
            ResetRequest {
                email: "kim@example.com",
                client: "203.0.113.13",
                token: fresh_token(80 + i as u8),
                now: NOW,
            },
        )
        .await
        .unwrap()
        {
            RequestOutcome::Accepted { .. } => {}
            _ => panic!("request {i} refused"),
        }
    }
    match account::request_reset(
        &store,
        &outbox,
        &LINKS,
        &cfg,
        ResetRequest {
            email: "kim@example.com",
            client: "203.0.113.99",
            token: fresh_token(90),
            now: NOW,
        },
    )
    .await
    .unwrap()
    {
        RequestOutcome::RateLimited { retry_after_secs } => assert!(retry_after_secs > 0),
        _ => panic!("the per-address limit did not bite"),
    }
    // Only the newest link works: each request retired the one before.
    let newest = outbox.last_token();
    assert!(matches!(
        account::complete_reset(
            &store,
            &outbox,
            &LINKS,
            &cfg,
            ResetCompletion {
                token: &newest,
                new_password: NEW_PW,
                salt: "c29tZXNhbHR2YWx1ZTE",
                now: NOW + 1
            }
        )
        .await
        .unwrap(),
        ResetOutcome::Done { .. }
    ));
}

#[tokio::test]
async fn changing_the_password_needs_the_old_one_and_keeps_the_current_session() {
    let store = store();
    let outbox = Outbox::default();
    let user = verified_account(&store, &outbox, "lee", "lee@example.com", 12).await;
    let cfg = recovery_config();

    // Two sessions: the one making the change, and another device.
    let mine = SessionToken::from_bytes([0x71; TOKEN_BYTES]);
    let other = SessionToken::from_bytes([0x72; TOKEN_BYTES]);
    for t in [&mine, &other] {
        store
            .create_session(&notespace_core::session::Session {
                token_hash: t.hash(),
                user_id: user.id,
                created_at: NOW,
                refreshed_at: NOW,
                expires_at: NOW + 1_000_000,
            })
            .await
            .unwrap();
    }
    let keep = store
        .lookup_session(&mine.hash(), NOW)
        .await
        .unwrap()
        .unwrap()
        .session;

    match account::change_password(
        &store,
        &outbox,
        &LINKS,
        &cfg,
        PasswordChange {
            user: &user,
            current: "wrong old",
            new_password: NEW_PW,
            salt: "c29tZXNhbHR2YWx1ZTE",
            keep: Some(&keep),
            now: NOW,
        },
    )
    .await
    .unwrap()
    {
        ChangeOutcome::WrongPassword => {}
        _ => panic!("changed without the old password"),
    }
    assert!(can_log_in(&store, "lee", PW, 120).await);

    match account::change_password(
        &store,
        &outbox,
        &LINKS,
        &cfg,
        PasswordChange {
            user: &user,
            current: PW,
            new_password: NEW_PW,
            salt: "c29tZXNhbHR2YWx1ZTE",
            keep: Some(&keep),
            now: NOW,
        },
    )
    .await
    .unwrap()
    {
        ChangeOutcome::Changed => {}
        _ => panic!("expected Changed"),
    }
    assert!(can_log_in(&store, "lee", NEW_PW, 121).await);
    assert!(
        store
            .lookup_session(&mine.hash(), NOW)
            .await
            .unwrap()
            .is_some(),
        "signed myself out"
    );
    assert!(
        store
            .lookup_session(&other.hash(), NOW)
            .await
            .unwrap()
            .is_none(),
        "the other device survived"
    );
    assert!(outbox.last().subject.contains("changed"));
}
