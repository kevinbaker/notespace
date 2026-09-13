//! The providers, as data: what to send them and how to read what comes back. A transport only
//! has to POST a body with some headers and hand the status and body back. Everything a
//! provider is particular about -- field names, where the key goes, what success looks like --
//! is here, where a unit test can pin it without a network.
//!
//! Cloudflare's own service is reachable two ways: through a Worker binding, which needs no
//! key and is not an HTTP request at all, and through its REST API. Only the REST form is here;
//! the binding is the target's business.

use super::{EmailAddress, EmailError, MailError, Message};
use crate::encoding::base64;

/// `"Name <address>"` or a bare address, as the operator configures it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sender {
    pub name: Option<String>,
    pub address: EmailAddress,
}

impl Sender {
    pub fn parse(input: &str) -> Result<Sender, EmailError> {
        let s = input.trim();
        if let Some((name, rest)) = s.split_once('<') {
            let Some(addr) = rest.strip_suffix('>') else {
                return Err(EmailError::Malformed);
            };
            let name = name.trim().trim_matches('"').trim();
            return Ok(Sender {
                name: (!name.is_empty()).then(|| name.to_string()),
                address: EmailAddress::parse(addr)?,
            });
        }
        Ok(Sender {
            name: None,
            address: EmailAddress::parse(s)?,
        })
    }

    /// The RFC 5322 form, for providers that take one string.
    pub fn header(&self) -> String {
        match &self.name {
            Some(n) => format!("{n} <{}>", self.address),
            None => self.address.to_string(),
        }
    }

    /// The domain, which is what Mailgun addresses its API by.
    pub fn domain(&self) -> &str {
        self.address
            .as_str()
            .rsplit_once('@')
            .map(|(_, d)| d)
            .unwrap_or("")
    }
}

/// Which HTTP API to speak. Each carries only what its endpoint or auth needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    /// Cloudflare Email Service over REST. The key is an API token with the email sending
    /// permission; `account_id` is in the URL.
    Cloudflare {
        account_id: String,
    },
    Resend,
    /// `stream` selects a Postmark message stream; `None` uses the server's default.
    Postmark {
        stream: Option<String>,
    },
    SendGrid,
    /// Mailgun addresses its API by sending domain, and EU accounts by a different host.
    Mailgun {
        domain: String,
        eu: bool,
    },
    Brevo,
}

/// What a transport POSTs.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboundRequest {
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Body,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Json(serde_json::Value),
    /// `application/x-www-form-urlencoded`; the transport encodes it.
    Form(Vec<(String, String)>),
}

impl Provider {
    /// The name the configuration uses.
    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Cloudflare { .. } => "cloudflare",
            Provider::Resend => "resend",
            Provider::Postmark { .. } => "postmark",
            Provider::SendGrid => "sendgrid",
            Provider::Mailgun { .. } => "mailgun",
            Provider::Brevo => "brevo",
        }
    }

    /// Build the request. `key` is the provider's API key or token; Mailgun's goes in basic
    /// auth, so it arrives pre-encoded by the caller through [`Provider::basic_auth`].
    pub fn request(&self, key: &str, from: &Sender, m: &Message) -> OutboundRequest {
        let json = |v: serde_json::Value| Body::Json(v);
        let bearer = |k: &str| vec![("Authorization", format!("Bearer {k}"))];
        match self {
            Provider::Cloudflare { account_id } => OutboundRequest {
                url: format!(
                    "https://api.cloudflare.com/client/v4/accounts/{account_id}/email/sending/send"
                ),
                headers: bearer(key),
                body: json(serde_json::json!({
                    "from": { "address": from.address.as_str(), "name": from.name },
                    "to": [m.to.as_str()],
                    "subject": m.subject,
                    "text": m.text,
                })),
            },
            Provider::Resend => OutboundRequest {
                url: "https://api.resend.com/emails".into(),
                headers: bearer(key),
                body: json(serde_json::json!({
                    "from": from.header(),
                    "to": [m.to.as_str()],
                    "subject": m.subject,
                    "text": m.text,
                })),
            },
            Provider::Postmark { stream } => {
                let mut body = serde_json::json!({
                    "From": from.header(),
                    "To": m.to.as_str(),
                    "Subject": m.subject,
                    "TextBody": m.text,
                });
                if let Some(s) = stream {
                    body["MessageStream"] = serde_json::Value::String(s.clone());
                }
                OutboundRequest {
                    url: "https://api.postmarkapp.com/email".into(),
                    headers: vec![
                        ("X-Postmark-Server-Token", key.to_string()),
                        ("Accept", "application/json".to_string()),
                    ],
                    body: json(body),
                }
            }
            Provider::SendGrid => OutboundRequest {
                url: "https://api.sendgrid.com/v3/mail/send".into(),
                headers: bearer(key),
                body: json(serde_json::json!({
                    "personalizations": [{ "to": [{ "email": m.to.as_str() }] }],
                    "from": { "email": from.address.as_str(), "name": from.name },
                    "subject": m.subject,
                    "content": [{ "type": "text/plain", "value": m.text }],
                })),
            },
            Provider::Mailgun { domain, eu } => OutboundRequest {
                url: format!(
                    "https://api{}.mailgun.net/v3/{domain}/messages",
                    if *eu { ".eu" } else { "" }
                ),
                headers: vec![("Authorization", format!("Basic {}", Self::basic_auth(key)))],
                body: Body::Form(vec![
                    ("from".into(), from.header()),
                    ("to".into(), m.to.as_str().to_string()),
                    ("subject".into(), m.subject.clone()),
                    ("text".into(), m.text.clone()),
                ]),
            },
            Provider::Brevo => OutboundRequest {
                url: "https://api.brevo.com/v3/smtp/email".into(),
                headers: vec![
                    ("api-key", key.to_string()),
                    ("Accept", "application/json".to_string()),
                ],
                body: json(serde_json::json!({
                    "sender": { "email": from.address.as_str(), "name": from.name },
                    "to": [{ "email": m.to.as_str() }],
                    "subject": m.subject,
                    "textContent": m.text,
                })),
            },
        }
    }

    /// `api:<key>`, base64, as Mailgun's basic auth wants it.
    pub fn basic_auth(key: &str) -> String {
        base64(format!("api:{key}").as_bytes())
    }

    /// Sort the reply into sent or refused. Every provider says something different on
    /// failure, so the message is whatever field it puts a reason in, or the raw body.
    pub fn parse_response(&self, status: u16, body: &str) -> Result<(), MailError> {
        let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
        let ok = (200..300).contains(&status)
            && match self {
                // A 2xx with an id, or Cloudflare's `success`: a 2xx alone is not a send.
                Provider::Cloudflare { .. } => v["success"].as_bool() == Some(true),
                Provider::Resend => v["id"].is_string(),
                Provider::Postmark { .. } => v["ErrorCode"].as_i64() == Some(0),
                // SendGrid answers 202 with no body.
                Provider::SendGrid => true,
                Provider::Mailgun { .. } => v["id"].is_string(),
                Provider::Brevo => v["messageId"].is_string(),
            };
        if ok {
            return Ok(());
        }
        let why = [
            &v["message"],
            &v["Message"],
            &v["errors"][0]["message"],
            &v["error"],
            &v["code"],
        ]
        .into_iter()
        .find_map(|f| f.as_str().map(str::to_string))
        .unwrap_or_else(|| {
            let raw = body.trim();
            if raw.is_empty() {
                "no body".to_string()
            } else {
                raw.chars().take(200).collect()
            }
        });
        Err(MailError::Rejected(format!(
            "{} {status}: {why}",
            self.as_str()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message() -> Message {
        Message {
            to: EmailAddress::parse("alice@example.com").unwrap(),
            subject: "hi".into(),
            text: "body".into(),
        }
    }

    fn sender() -> Sender {
        Sender::parse("forum <no-reply@forum.example>").unwrap()
    }

    #[test]
    fn senders_parse_with_or_without_a_name() {
        let s = sender();
        assert_eq!(s.name.as_deref(), Some("forum"));
        assert_eq!(s.address.as_str(), "no-reply@forum.example");
        assert_eq!(s.header(), "forum <no-reply@forum.example>");
        assert_eq!(s.domain(), "forum.example");
        let bare = Sender::parse("no-reply@forum.example").unwrap();
        assert_eq!(bare.name, None);
        assert_eq!(bare.header(), "no-reply@forum.example");
        let quoted = Sender::parse("\"The Forum\" <No-Reply@Forum.Example>").unwrap();
        assert_eq!(quoted.name.as_deref(), Some("The Forum"));
        assert_eq!(quoted.address.as_str(), "no-reply@forum.example");
        for bad in ["", "forum <", "forum <nope>", "<>"] {
            assert!(Sender::parse(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn cloudflare_uses_address_not_email_and_reads_success() {
        let p = Provider::Cloudflare {
            account_id: "acct".into(),
        };
        let r = p.request("tok", &sender(), &message());
        assert_eq!(
            r.url,
            "https://api.cloudflare.com/client/v4/accounts/acct/email/sending/send"
        );
        assert!(r.headers.contains(&("Authorization", "Bearer tok".into())));
        let Body::Json(b) = r.body else {
            panic!("json")
        };
        assert_eq!(b["from"]["address"], "no-reply@forum.example");
        assert_eq!(b["from"]["name"], "forum");
        assert!(b["from"].get("email").is_none(), "REST uses `address`");
        assert_eq!(b["to"], serde_json::json!(["alice@example.com"]));
        assert_eq!(b["text"], "body");
        assert_eq!(
            p.parse_response(
                200,
                r#"{"success":true,"errors":[],"result":{"delivered":["alice@example.com"]}}"#
            ),
            Ok(())
        );
        let refused = p.parse_response(
            400,
            r#"{"success":false,"errors":[{"code":1000,"message":"Sender domain not verified"}],"result":null}"#,
        );
        assert!(
            matches!(&refused, Err(MailError::Rejected(w)) if w.contains("Sender domain not verified"))
        );
        // A 200 that says success=false is still a refusal.
        assert!(p
            .parse_response(200, r#"{"success":false,"errors":[]}"#)
            .is_err());
    }

    #[test]
    fn resend_takes_one_from_string_and_answers_with_an_id() {
        let r = Provider::Resend.request("re_1", &sender(), &message());
        assert_eq!(r.url, "https://api.resend.com/emails");
        let Body::Json(b) = r.body else {
            panic!("json")
        };
        assert_eq!(b["from"], "forum <no-reply@forum.example>");
        assert_eq!(b["to"], serde_json::json!(["alice@example.com"]));
        assert!(b.get("html").is_none(), "plain text only");
        assert_eq!(
            Provider::Resend.parse_response(200, r#"{"id":"abc"}"#),
            Ok(())
        );
        assert!(Provider::Resend.parse_response(200, "{}").is_err());
        match Provider::Resend.parse_response(
            422,
            r#"{"statusCode":422,"message":"The from domain is not verified"}"#,
        ) {
            Err(MailError::Rejected(w)) => assert!(w.contains("422") && w.contains("not verified")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn postmark_uses_its_own_casing_and_error_code_zero() {
        let p = Provider::Postmark {
            stream: Some("outbound".into()),
        };
        let r = p.request("srv", &sender(), &message());
        assert!(r
            .headers
            .contains(&("X-Postmark-Server-Token", "srv".into())));
        let Body::Json(b) = r.body else {
            panic!("json")
        };
        assert_eq!(b["From"], "forum <no-reply@forum.example>");
        assert_eq!(b["To"], "alice@example.com");
        assert_eq!(b["TextBody"], "body");
        assert_eq!(b["MessageStream"], "outbound");
        let no_stream = Provider::Postmark { stream: None }.request("s", &sender(), &message());
        let Body::Json(b) = no_stream.body else {
            panic!()
        };
        assert!(b.get("MessageStream").is_none());
        assert_eq!(
            p.parse_response(200, r#"{"ErrorCode":0,"Message":"OK","MessageID":"x"}"#),
            Ok(())
        );
        match p.parse_response(
            422,
            r#"{"ErrorCode":300,"Message":"Invalid 'From' address"}"#,
        ) {
            Err(MailError::Rejected(w)) => assert!(w.contains("Invalid 'From'")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn sendgrid_nests_recipients_and_answers_202_with_no_body() {
        let r = Provider::SendGrid.request("SG.x", &sender(), &message());
        let Body::Json(b) = r.body else {
            panic!("json")
        };
        assert_eq!(
            b["personalizations"][0]["to"][0]["email"],
            "alice@example.com"
        );
        assert_eq!(b["from"]["email"], "no-reply@forum.example");
        assert_eq!(b["content"][0]["type"], "text/plain");
        assert_eq!(Provider::SendGrid.parse_response(202, ""), Ok(()));
        match Provider::SendGrid.parse_response(401, r#"{"errors":[{"message":"The provided authorization grant is invalid","field":null}]}"#) {
            Err(MailError::Rejected(w)) => assert!(w.contains("authorization grant")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mailgun_is_a_form_post_with_basic_auth_by_domain() {
        let p = Provider::Mailgun {
            domain: "forum.example".into(),
            eu: false,
        };
        let r = p.request("key-1", &sender(), &message());
        assert_eq!(r.url, "https://api.mailgun.net/v3/forum.example/messages");
        // `api:key-1`
        assert!(r
            .headers
            .contains(&("Authorization", "Basic YXBpOmtleS0x".into())));
        let Body::Form(f) = r.body else {
            panic!("form")
        };
        assert!(f.contains(&("to".into(), "alice@example.com".into())));
        assert!(f.contains(&("from".into(), "forum <no-reply@forum.example>".into())));
        let eu = Provider::Mailgun {
            domain: "forum.example".into(),
            eu: true,
        };
        assert!(eu
            .request("k", &sender(), &message())
            .url
            .starts_with("https://api.eu.mailgun.net/"));
        assert_eq!(
            p.parse_response(
                200,
                r#"{"id":"<x@forum.example>","message":"Queued. Thank you."}"#
            ),
            Ok(())
        );
        match p.parse_response(401, "Forbidden") {
            Err(MailError::Rejected(w)) => assert!(w.contains("Forbidden")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn brevo_uses_sender_and_text_content() {
        let r = Provider::Brevo.request("xkeysib-1", &sender(), &message());
        assert!(r.headers.contains(&("api-key", "xkeysib-1".into())));
        let Body::Json(b) = r.body else {
            panic!("json")
        };
        assert_eq!(b["sender"]["email"], "no-reply@forum.example");
        assert_eq!(b["to"][0]["email"], "alice@example.com");
        assert_eq!(b["textContent"], "body");
        assert_eq!(
            Provider::Brevo.parse_response(201, r#"{"messageId":"<1@smtp-relay>"}"#),
            Ok(())
        );
        match Provider::Brevo.parse_response(
            400,
            r#"{"code":"invalid_parameter","message":"sender not valid"}"#,
        ) {
            Err(MailError::Rejected(w)) => assert!(w.contains("sender not valid")),
            other => panic!("{other:?}"),
        }
    }
}
