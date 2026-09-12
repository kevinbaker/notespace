//! Email: addresses, the messages this system sends, and the seam they leave through.
//!
//! Nothing here sends. [`Mailer`] is implemented by the target -- a Worker over a provider's
//! HTTP API or Cloudflare's own binding, a native binary over whatever it likes -- and the rest
//! of `core` builds [`Message`]s and hands them over. The providers' request and response
//! shapes are in [`providers`], so they are tested here rather than in a Worker. Templates are plain text: they are read on phones, in terminals, and by
//! people who have just lost a password, and none of those wants a layout.

use core::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{Timestamp, UserId};
use crate::session::{hex_decode, hex_encode};

pub mod providers;

/// Longest address accepted, per RFC 5321's path limit.
pub const MAX_ADDRESS_CHARS: usize = 254;

/// An address that passed [`EmailAddress::parse`]: trimmed, lowercased, one `@`, a dotted
/// domain. Deliverability is not checked here; the verification mail is what checks it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EmailAddress(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EmailError {
    #[error("enter an email address")]
    Empty,
    #[error("that does not look like an email address")]
    Malformed,
    #[error("that address is too long")]
    TooLong,
}

impl EmailAddress {
    /// Deliberately loose: the rules that matter are "one `@`, something either side, a dot in
    /// the domain, no whitespace". Anything stricter rejects addresses that work.
    pub fn parse(input: &str) -> Result<Self, EmailError> {
        let s = input.trim();
        if s.is_empty() {
            return Err(EmailError::Empty);
        }
        if s.chars().count() > MAX_ADDRESS_CHARS {
            return Err(EmailError::TooLong);
        }
        let Some((local, domain)) = s.rsplit_once('@') else {
            return Err(EmailError::Malformed);
        };
        let clean = |part: &str| {
            !part.is_empty()
                && !part.chars().any(|c| {
                    c.is_whitespace()
                        || c.is_control()
                        || c == '@'
                        || c == '<'
                        || c == '>'
                        || c == ','
                })
        };
        if !clean(local) || !clean(domain) {
            return Err(EmailError::Malformed);
        }
        if !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.') {
            return Err(EmailError::Malformed);
        }
        Ok(EmailAddress(s.to_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for EmailAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One outbound mail. Plain text only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub to: EmailAddress,
    pub subject: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MailError {
    /// No transport is configured. Callers treat this as "the mail did not go", never as fatal.
    #[error("no mailer is configured")]
    NotConfigured,
    /// The provider refused the message; the status and its body, for the log.
    #[error("mail provider refused: {0}")]
    Rejected(String),
    /// Could not reach the provider.
    #[error("mail transport failed: {0}")]
    Transport(String),
}

/// The seam. `?Send` because wasm futures are not `Send`.
#[async_trait::async_trait(?Send)]
pub trait Mailer {
    async fn send(&self, message: &Message) -> Result<(), MailError>;
}

/// No transport. Every send reports [`MailError::NotConfigured`].
pub struct NoMailer;

#[async_trait::async_trait(?Send)]
impl Mailer for NoMailer {
    async fn send(&self, _message: &Message) -> Result<(), MailError> {
        Err(MailError::NotConfigured)
    }
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

/// Bytes of randomness in a link token. Same strength as a session.
pub const TOKEN_BYTES: usize = 32;

/// What a link token is for. Stored, and matched on consumption, so a verification link cannot
/// be spent as a reset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Verify,
    Reset,
}

impl TokenKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            TokenKind::Verify => "verify",
            TokenKind::Reset => "reset",
        }
    }

    /// How long a link of this kind works. Reset links are short-lived because the mailbox
    /// they land in is the weakest link in the chain.
    pub const fn lifetime_ms(&self) -> i64 {
        match self {
            TokenKind::Verify => 24 * 60 * 60 * 1000,
            TokenKind::Reset => 60 * 60 * 1000,
        }
    }
}

/// The secret in the link. Never stored; the database sees [`EmailToken::hash`] only.
#[derive(Clone, PartialEq, Eq)]
pub struct EmailToken([u8; TOKEN_BYTES]);

impl EmailToken {
    /// The caller supplies the bytes, because `core` has no RNG.
    pub fn from_bytes(bytes: [u8; TOKEN_BYTES]) -> Self {
        EmailToken(bytes)
    }

    /// Parse the value out of a link. Rejects anything not exactly the right length.
    pub fn parse(s: &str) -> Option<Self> {
        let bytes = hex_decode(s)?;
        (bytes.len() == TOKEN_BYTES).then(|| {
            let mut buf = [0u8; TOKEN_BYTES];
            buf.copy_from_slice(&bytes);
            EmailToken(buf)
        })
    }

    /// The value that goes in the link.
    pub fn as_link_value(&self) -> String {
        hex_encode(&self.0)
    }

    /// The only route from a token to something storable.
    pub fn hash(&self) -> String {
        hex_encode(&Sha256::digest(self.0))
    }
}

impl fmt::Debug for EmailToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EmailToken(<redacted>)")
    }
}

/// A stored link token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredToken {
    pub token_hash: String,
    pub user_id: UserId,
    pub kind: TokenKind,
    pub email: EmailAddress,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
}

/// What consuming a token yields: whose it was and which address it proves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumedToken {
    pub user_id: UserId,
    pub email: EmailAddress,
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Where links point. `base_url` has no trailing slash: `https://forum.example`.
pub struct Links<'a> {
    pub base_url: &'a str,
    pub site_name: &'a str,
}

impl Links<'_> {
    pub fn verify(&self, token: &EmailToken) -> String {
        format!("{}/verify?token={}", self.base_url, token.as_link_value())
    }

    pub fn reset(&self, token: &EmailToken) -> String {
        format!("{}/reset?token={}", self.base_url, token.as_link_value())
    }
}

/// Sent after signup or an address change. Following the link is the proof.
pub fn verification(
    links: &Links<'_>,
    to: &EmailAddress,
    username: &str,
    token: &EmailToken,
) -> Message {
    Message {
        to: to.clone(),
        subject: format!("Confirm your email for {}", links.site_name),
        text: format!(
            "Hi {username},\n\n\
             Someone -- probably you -- gave this address to {site} for the account \"{username}\".\n\
             To confirm it, open this link:\n\n    {link}\n\n\
             The link works once and for a day. If this was not you, ignore this message; \
             nothing will happen and nobody can use this address without following the link.\n",
            site = links.site_name,
            link = links.verify(token),
        ),
    }
}

/// Sent on a reset request for a verified address.
pub fn password_reset(
    links: &Links<'_>,
    to: &EmailAddress,
    username: &str,
    token: &EmailToken,
) -> Message {
    Message {
        to: to.clone(),
        subject: format!("Reset your {} password", links.site_name),
        text: format!(
            "Hi {username},\n\n\
             A password reset was requested for your {site} account. To choose a new password, \
             open this link within the hour:\n\n    {link}\n\n\
             If you did not ask for this, ignore it. Your password has not changed, and the link \
             does nothing on its own.\n",
            site = links.site_name,
            link = links.reset(token),
        ),
    }
}

/// Sent to the verified address when the password changes, by reset or from settings. A user
/// who did not do this needs to know.
pub fn password_changed(links: &Links<'_>, to: &EmailAddress, username: &str) -> Message {
    Message {
        to: to.clone(),
        subject: format!("Your {} password was changed", links.site_name),
        text: format!(
            "Hi {username},\n\n\
             The password for your {site} account was just changed, and every other signed-in \
             session was ended.\n\n\
             If that was you, there is nothing to do. If it was not, reset it now from \
             {base}/forgot -- the reset goes to this address, which is still yours.\n",
            site = links.site_name,
            base = links.base_url,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_trimmed_and_lowercased() {
        let a = EmailAddress::parse("  Alice@Example.COM \n").unwrap();
        assert_eq!(a.as_str(), "alice@example.com");
    }

    #[test]
    fn malformed_addresses_are_refused() {
        for bad in [
            "",
            " ",
            "alice",
            "alice@",
            "@example.com",
            "alice@localhost",
            "alice@.com",
            "alice@example.",
            "al ice@example.com",
            "alice@exa mple.com",
            "alice@@example.com",
            "<alice@example.com>",
            "alice@example.com,bob@example.com",
            "alice\n@example.com",
        ] {
            assert!(EmailAddress::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn plus_addressing_and_subdomains_are_fine() {
        for good in [
            "alice+forum@example.com",
            "a.b.c@mail.example.co.uk",
            "o'neil@example.com",
            "\"quoted\"@example.com",
        ] {
            assert!(EmailAddress::parse(good).is_ok(), "refused {good:?}");
        }
    }

    #[test]
    fn an_overlong_address_is_refused() {
        let long = format!("{}@example.com", "a".repeat(MAX_ADDRESS_CHARS));
        assert_eq!(EmailAddress::parse(&long), Err(EmailError::TooLong));
    }

    fn links() -> Links<'static> {
        Links {
            base_url: "https://forum.example",
            site_name: "forum",
        }
    }

    fn token() -> EmailToken {
        EmailToken::from_bytes([0xAB; TOKEN_BYTES])
    }

    #[test]
    fn tokens_round_trip_through_the_link_and_never_store_themselves() {
        let t = token();
        let value = t.as_link_value();
        assert_eq!(EmailToken::parse(&value), Some(t.clone()));
        assert_ne!(t.hash(), value);
        assert_eq!(t.hash().len(), 64);
        assert!(!format!("{t:?}").contains("abab"));
        for bad in ["", "ab", &"a".repeat(63), &"g".repeat(64)] {
            assert!(EmailToken::parse(bad).is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn the_verification_mail_carries_the_link_and_the_name() {
        let to = EmailAddress::parse("alice@example.com").unwrap();
        let m = verification(&links(), &to, "alice", &token());
        assert!(m.text.contains("https://forum.example/verify?token="));
        assert!(m.text.contains(&token().as_link_value()));
        assert!(m.text.contains("alice"));
        assert!(m.subject.contains("forum"));
    }

    #[test]
    fn the_reset_mail_points_at_reset_and_says_it_expires() {
        let to = EmailAddress::parse("alice@example.com").unwrap();
        let m = password_reset(&links(), &to, "alice", &token());
        assert!(m.text.contains("https://forum.example/reset?token="));
        assert!(m.text.contains("hour"));
    }

    /// The link is the secret; it must appear nowhere but the mail to its owner.
    #[test]
    fn the_changed_notice_carries_no_token() {
        let to = EmailAddress::parse("alice@example.com").unwrap();
        let m = password_changed(&links(), &to, "alice");
        assert!(!m.text.contains("token="));
        assert!(m.text.contains("/forgot"));
    }
}
