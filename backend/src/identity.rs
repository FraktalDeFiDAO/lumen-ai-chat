//! Who is asking.
//!
//! Lumen has had exactly one user for its whole life: whoever could reach
//! loopback. Everything downstream assumed that — one conversation store in one
//! browser, one workspace, one budget. Serving more than one person starts here,
//! with a name for the person on the other end of a request.
//!
//! Identity is resolved from a header set by a trusted reverse proxy rather than
//! from credentials this service checks itself. That is not a shortcut. An
//! identity-aware proxy is what the deployment already documents, it is what
//! terminates TLS, and password handling is the part of this problem with the
//! worst failure mode and the least novelty. `Mode::Proxy` is therefore only
//! safe when nothing can reach the gateway except that proxy, which is why the
//! loopback guard stays and why this module refuses to guess.
//!
//! The trait is the point: a future `Mode::Accounts` that verifies credentials
//! here changes this file and nothing else.

use std::sync::Arc;

use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, StatusCode, request::Parts},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The identifier reserved for the single-user deployment, so a store written
/// before identity existed keeps working and keeps belonging to someone.
pub const LOCAL_USER: &str = "local";

/// A user id becomes a database key and appears in logs; it is not free-form.
const MAX_USER_ID_BYTES: usize = 128;
const MAX_DISPLAY_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// Stable, opaque, and the only field anything is keyed by.
    pub id: String,
    pub display_name: Option<String>,
    pub email: Option<String>,
}

impl User {
    pub fn local() -> Self {
        Self {
            id: LOCAL_USER.into(),
            display_name: None,
            email: None,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdentityError {
    #[error("the request carried no identity")]
    Missing,
    #[error("the identity header is not usable: {0}")]
    Malformed(String),
    #[error("the request did not come from the configured proxy")]
    Untrusted,
}

impl IntoResponse for IdentityError {
    fn into_response(self) -> Response {
        // Unauthenticated rather than forbidden: the proxy in front is what
        // authenticates, so a request arriving without an identity has not been
        // through it, and retrying with credentials is the fix.
        let status = StatusCode::UNAUTHORIZED;
        let body = axum::Json(serde_json::json!({
            "error": { "type": "unauthenticated", "message": self.to_string() }
        }));
        (status, body).into_response()
    }
}

/// How a request is turned into a user.
pub trait IdentityProvider: Send + Sync + std::fmt::Debug {
    fn identify(&self, headers: &HeaderMap) -> Result<User, IdentityError>;

    /// Whether this deployment serves more than one person. The single-user
    /// mode keeps behaviour that only makes sense for one.
    fn is_multi_user(&self) -> bool {
        true
    }

    /// Whether this provider can prove a request came from the proxy, rather
    /// than only believing what it was told.
    fn has_shared_secret(&self) -> bool {
        false
    }
}

/// Everyone is the same person: the behaviour Lumen has always had.
#[derive(Debug, Clone, Copy, Default)]
pub struct SingleUser;

impl IdentityProvider for SingleUser {
    fn identify(&self, _headers: &HeaderMap) -> Result<User, IdentityError> {
        Ok(User::local())
    }

    fn is_multi_user(&self) -> bool {
        false
    }
}

/// Identity asserted by a reverse proxy.
#[derive(Debug, Clone)]
pub struct ProxyHeaders {
    pub user_header: String,
    pub email_header: Option<String>,
    pub name_header: Option<String>,
    /// A secret only the proxy knows, if one is configured.
    ///
    /// Without it, trusting the identity header is a claim about the network:
    /// anyone who can reach the port can say they are anyone. The service cannot
    /// check the network, but it can check that whoever sent the request knows
    /// something only the proxy was told -- which turns "nothing else can reach
    /// this port" from an assertion into something that fails closed when it is
    /// wrong.
    pub secret: Option<Secret>,
}

/// A shared secret and the header it arrives in.
#[derive(Clone)]
pub struct Secret {
    pub header: String,
    value: Vec<u8>,
}

impl std::fmt::Debug for Secret {
    /// Never the value: this ends up in startup logs and error reports.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Secret")
            .field("header", &self.header)
            .field("value", &"<redacted>")
            .finish()
    }
}

impl Secret {
    pub fn new(header: impl Into<String>, value: impl AsRef<[u8]>) -> Self {
        Self {
            header: header.into(),
            value: value.as_ref().to_vec(),
        }
    }

    /// Compared in constant time. A byte-at-a-time comparison leaks the secret
    /// to anyone who can measure how long a rejection took.
    fn matches(&self, presented: &[u8]) -> bool {
        if presented.len() != self.value.len() {
            return false;
        }
        let mut difference = 0u8;
        for (expected, actual) in self.value.iter().zip(presented) {
            difference |= expected ^ actual;
        }
        difference == 0
    }
}

impl Default for ProxyHeaders {
    fn default() -> Self {
        Self {
            user_header: "x-forwarded-user".into(),
            email_header: Some("x-forwarded-email".into()),
            name_header: Some("x-forwarded-preferred-username".into()),
            secret: None,
        }
    }
}

impl IdentityProvider for ProxyHeaders {
    fn has_shared_secret(&self) -> bool {
        self.secret.is_some()
    }

    fn identify(&self, headers: &HeaderMap) -> Result<User, IdentityError> {
        // Checked before anything is read from the request: a caller who cannot
        // prove it is the proxy has not identified anybody, whatever it claims.
        if let Some(secret) = &self.secret {
            let presented = headers
                .get(&secret.header)
                .map(|value| value.as_bytes())
                .unwrap_or_default();
            if !secret.matches(presented) {
                return Err(IdentityError::Untrusted);
            }
        }
        let raw = headers
            .get(&self.user_header)
            .ok_or(IdentityError::Missing)?
            .to_str()
            .map_err(|_| IdentityError::Malformed("the identity is not text".into()))?;

        Ok(User {
            id: validate_id(raw)?,
            display_name: self
                .name_header
                .as_ref()
                .and_then(|name| optional_field(headers, name)),
            email: self
                .email_header
                .as_ref()
                .and_then(|name| optional_field(headers, name)),
        })
    }
}

/// A user id is a storage key, so it is bounded and free of anything that would
/// change meaning somewhere downstream: no separators a path or a log line
/// would read structurally, no control characters, no surrounding space.
fn validate_id(raw: &str) -> Result<String, IdentityError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(IdentityError::Malformed("the identity is empty".into()));
    }
    if trimmed.len() > MAX_USER_ID_BYTES {
        return Err(IdentityError::Malformed(format!(
            "the identity is longer than {MAX_USER_ID_BYTES} bytes"
        )));
    }
    if trimmed.chars().any(|character| {
        character.is_control()
            || character.is_whitespace()
            || matches!(character, '/' | '\\' | '"' | '\'' | '\0')
    }) {
        return Err(IdentityError::Malformed(
            "the identity contains characters that are not allowed".into(),
        ));
    }
    Ok(trimmed.to_owned())
}

/// A display field is decoration: unusable content is dropped rather than
/// failing a request that is otherwise perfectly identified.
fn optional_field(headers: &HeaderMap, name: &str) -> Option<String> {
    let value = headers.get(name)?.to_str().ok()?.trim();
    if value.is_empty() || value.len() > MAX_DISPLAY_BYTES || value.chars().any(char::is_control) {
        return None;
    }
    Some(value.to_owned())
}

/// The configured provider, resolved once at startup.
#[derive(Debug, Clone)]
pub struct Identity(Arc<dyn IdentityProvider>);

impl Identity {
    pub fn single_user() -> Self {
        Self(Arc::new(SingleUser))
    }

    pub fn proxy(headers: ProxyHeaders) -> Self {
        Self(Arc::new(headers))
    }

    pub fn identify(&self, headers: &HeaderMap) -> Result<User, IdentityError> {
        self.0.identify(headers)
    }

    pub fn is_multi_user(&self) -> bool {
        self.0.is_multi_user()
    }

    pub fn has_shared_secret(&self) -> bool {
        self.0.has_shared_secret()
    }
}

impl Default for Identity {
    fn default() -> Self {
        Self::single_user()
    }
}

/// The user a handler is acting for.
///
/// Extracting this is what makes a handler user-aware; a handler that does not
/// ask for it is, by construction, serving everyone the same thing.
#[derive(Debug, Clone)]
pub struct CurrentUser(pub User);

impl<S> FromRequestParts<S> for CurrentUser
where
    S: Send + Sync,
    Identity: axum::extract::FromRef<S>,
{
    type Rejection = IdentityError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let identity = <Identity as axum::extract::FromRef<S>>::from_ref(state);
        identity.identify(&parts.headers).map(CurrentUser)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    #[test]
    fn the_single_user_deployment_answers_for_everyone() {
        let provider = SingleUser;
        assert_eq!(provider.identify(&HeaderMap::new()).unwrap(), User::local());
        assert_eq!(
            provider
                .identify(&headers(&[("x-forwarded-user", "someone")]))
                .unwrap(),
            User::local(),
            "a header nobody configured must not be able to change who this is"
        );
        assert!(!provider.is_multi_user());
    }

    #[test]
    fn a_proxy_identity_carries_its_display_fields() {
        let user = ProxyHeaders::default()
            .identify(&headers(&[
                ("x-forwarded-user", "riley"),
                ("x-forwarded-email", "riley@example.com"),
                ("x-forwarded-preferred-username", "Riley Chen"),
            ]))
            .unwrap();

        assert_eq!(user.id, "riley");
        assert_eq!(user.email.as_deref(), Some("riley@example.com"));
        assert_eq!(user.display_name.as_deref(), Some("Riley Chen"));
    }

    #[test]
    fn an_identity_is_required_when_a_proxy_is_meant_to_supply_it() {
        assert_eq!(
            ProxyHeaders::default().identify(&HeaderMap::new()),
            Err(IdentityError::Missing)
        );
    }

    #[test]
    fn an_unusable_identity_is_refused_rather_than_repaired() {
        let provider = ProxyHeaders::default();
        for value in [
            "",
            "   ",
            "with space",
            "a/b",
            "a\\b",
            "quote\"d",
            "tab\there",
        ] {
            let result = provider.identify(&headers(&[("x-forwarded-user", value)]));
            assert!(
                matches!(result, Err(IdentityError::Malformed(_))),
                "{value:?} should not be accepted as an identity, got {result:?}"
            );
        }

        let long = "x".repeat(MAX_USER_ID_BYTES + 1);
        assert!(matches!(
            provider.identify(&headers(&[("x-forwarded-user", &long)])),
            Err(IdentityError::Malformed(_))
        ));
    }

    #[test]
    fn surrounding_space_is_trimmed_rather_than_making_two_users_of_one() {
        let user = ProxyHeaders::default()
            .identify(&headers(&[("x-forwarded-user", "  riley  ")]))
            .unwrap();
        assert_eq!(user.id, "riley");
    }

    #[test]
    fn a_missing_or_unusable_display_field_does_not_fail_the_request() {
        let user = ProxyHeaders::default()
            .identify(&headers(&[
                ("x-forwarded-user", "riley"),
                ("x-forwarded-email", "   "),
            ]))
            .unwrap();

        assert_eq!(user.id, "riley");
        assert_eq!(user.email, None);
        assert_eq!(user.display_name, None);
    }

    #[test]
    fn display_fields_are_bounded_like_the_identity_is() {
        let long = "x".repeat(MAX_DISPLAY_BYTES + 1);
        let user = ProxyHeaders::default()
            .identify(&headers(&[
                ("x-forwarded-user", "riley"),
                ("x-forwarded-preferred-username", &long),
            ]))
            .unwrap();
        assert_eq!(user.display_name, None);
    }

    #[test]
    fn a_deployment_can_name_its_own_headers() {
        let provider = ProxyHeaders {
            user_header: "x-authentik-uid".into(),
            email_header: None,
            name_header: None,
            secret: None,
        };
        let user = provider
            .identify(&headers(&[("x-authentik-uid", "abc123")]))
            .unwrap();

        assert_eq!(user.id, "abc123");
        assert_eq!(user.email, None);
    }

    #[test]
    fn an_unauthenticated_request_is_answered_as_json() {
        let response = IdentityError::Missing.into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
