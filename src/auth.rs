// SPDX-FileCopyrightText: 2026 Lorena Jiménez, Luis Liñán
// SPDX-License-Identifier: MIT

//! Access links: only invited people get into the Radio.
//!
//! Opening an Access link (`/?token=<uuid>`) stores its token in a cookie; every request must
//! carry a cookie with a configured token, checked in constant time.

use std::sync::Arc;

use anyhow::{Context, bail};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use subtle::ConstantTimeEq;
use url::Url;
use uuid::Uuid;

/// The cookie that holds the token of the Access link that was opened.
const COOKIE: &str = "bt_token";

/// What anyone without a valid Access link sees.
const DENIED: &str = "<!doctype html><title>bt</title><p>bt is private. Ask for your access link.";

/// The configured Access links and where Listeners reach bt.
pub struct Access {
    /// Each invited person's name and token.
    tokens: Vec<(String, Uuid)>,
    /// The origin Listeners' pages load from, e.g. `https://bt.example.com`.
    origin: String,
    /// Whether the cookie is only sent over HTTPS.
    secure: bool,
}

/// Configuration and checks.
impl Access {
    /// Builds the Access links from `BT_ACCESS_TOKENS` and `BT_PUBLIC_URL`; `None` if there are
    /// no tokens, which leaves bt open to anyone.
    ///
    /// # Errors
    ///
    /// Fails if the tokens are invalid (see [`parse`]) or the public URL is missing or isn't
    /// `http(s)`.
    pub fn new(tokens: Option<&str>, public_url: Option<&str>) -> anyhow::Result<Option<Self>> {
        let Some(tokens) = tokens else {
            return Ok(None);
        };
        let tokens = parse(tokens).context("BT_ACCESS_TOKENS")?;
        let url = public_url.context("BT_PUBLIC_URL is required with BT_ACCESS_TOKENS")?;
        let url = Url::parse(url).context("BT_PUBLIC_URL must be a URL")?;
        if !matches!(url.scheme(), "http" | "https") {
            bail!("BT_PUBLIC_URL must be http or https");
        }
        Ok(Some(Self {
            tokens,
            origin: url.origin().ascii_serialization(),
            secure: url.scheme() == "https",
        }))
    }

    /// Returns each invited person's name and Access link.
    pub fn links(&self) -> impl Iterator<Item = (&str, String)> {
        self.tokens
            .iter()
            .map(|(name, token)| (name.as_str(), format!("{}/?token={token}", self.origin)))
    }

    /// Returns `token` as a UUID if it is a configured one, compared in constant time against
    /// all of them.
    fn grants(&self, token: &str) -> Option<Uuid> {
        let token = Uuid::parse_str(token).ok()?;
        let found = self
            .tokens
            .iter()
            .fold(subtle::Choice::from(0), |found, (_, t)| {
                found | t.as_bytes().ct_eq(token.as_bytes())
            });
        bool::from(found).then_some(token)
    }
}

/// Parses `name:uuid,name:uuid`, one entry per invited person.
///
/// # Errors
///
/// Fails if there are no entries, an entry has no name or no valid UUID, or a name or UUID
/// appears twice. Errors never include a UUID.
fn parse(s: &str) -> anyhow::Result<Vec<(String, Uuid)>> {
    if s.trim().is_empty() {
        bail!("no entries");
    }
    let mut tokens: Vec<(String, Uuid)> = Vec::new();
    for (i, entry) in s.split(',').map(str::trim).enumerate() {
        let Some((name, token)) = entry.split_once(':') else {
            bail!("entry {} must be name:uuid", i + 1);
        };
        let name = name.trim();
        if name.is_empty() {
            bail!("entry {} has no name", i + 1);
        }
        let Ok(token) = Uuid::parse_str(token.trim()) else {
            bail!("{name}'s token is not a UUID");
        };
        if tokens.iter().any(|(n, _)| n == name) {
            bail!("{name} appears twice");
        }
        if tokens.iter().any(|(_, t)| *t == token) {
            bail!("{name} reuses another entry's token");
        }
        tokens.push((name.into(), token));
    }
    Ok(tokens)
}

/// Lets a request through only with a valid Access link.
///
/// `/?token=<uuid>` with a valid token sets the cookie and redirects to `/`; any other request
/// without a valid cookie gets 401. `/ws` must also come from a page on the public origin, or it
/// gets 403.
pub async fn check(State(access): State<Arc<Access>>, req: Request, next: Next) -> Response {
    let denied = (StatusCode::UNAUTHORIZED, Html(DENIED)).into_response();
    let uri = req.uri();
    if uri.path() == "/"
        && let Some(token) = query_token(uri.query().unwrap_or_default())
    {
        let Some(token) = access.grants(&token) else {
            return denied;
        };
        let secure = if access.secure { "; Secure" } else { "" };
        let cookie =
            format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age=31536000{secure}");
        return (
            StatusCode::SEE_OTHER,
            [(header::SET_COOKIE, cookie), (header::LOCATION, "/".into())],
        )
            .into_response();
    }
    if !cookie_token(req.headers())
        .and_then(|t| access.grants(t))
        .is_some()
    {
        return denied;
    }
    if uri.path() == "/ws"
        && req.headers().get(header::ORIGIN).map(|o| o.as_bytes()) != Some(access.origin.as_bytes())
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(req).await
}

/// Returns the `token` parameter of `query`, if any.
fn query_token(query: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.into_owned())
}

/// Returns the value of the [`COOKIE`] cookie, if any.
fn cookie_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(COOKIE)?.strip_prefix('='))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid token.
    const A: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";
    /// Another valid token.
    const B: &str = "a1a2a3a4-b1b2-c1c2-d1d2-d3d4d5d6d7d8";

    #[test]
    fn parses_entries() {
        let tokens = parse(&format!(" lorena:{A} , luis : {B} ")).unwrap();
        assert_eq!(
            tokens,
            [
                ("lorena".into(), Uuid::parse_str(A).unwrap()),
                ("luis".into(), Uuid::parse_str(B).unwrap()),
            ]
        );
    }

    #[test]
    fn rejects_invalid_entries_without_showing_tokens() {
        for (input, error) in [
            ("".to_string(), "no entries"),
            (" ".into(), "no entries"),
            (format!("lorena:{A},"), "entry 2 must be name:uuid"),
            (A.into(), "entry 1 must be name:uuid"),
            (format!(":{A}"), "entry 1 has no name"),
            ("lorena:not-a-uuid".into(), "lorena's token is not a UUID"),
            (format!("lorena:{A},lorena:{B}"), "lorena appears twice"),
            (
                format!("lorena:{A},luis:{A}"),
                "luis reuses another entry's token",
            ),
        ] {
            assert_eq!(parse(&input).unwrap_err().to_string(), error, "{input:?}");
        }
    }

    #[test]
    fn needs_a_public_url_only_with_tokens() {
        assert!(Access::new(None, None).unwrap().is_none());
        let tokens = format!("lorena:{A}");
        assert!(Access::new(Some(&tokens), None).is_err());
        assert!(Access::new(Some(&tokens), Some("ftp://bt.example.com")).is_err());
        let access = Access::new(Some(&tokens), Some("https://bt.example.com/x"))
            .unwrap()
            .unwrap();
        assert!(access.secure);
        assert_eq!(
            access.links().collect::<Vec<_>>(),
            [("lorena", format!("https://bt.example.com/?token={A}"))]
        );
    }

    #[test]
    fn grants_only_configured_tokens() {
        let tokens = format!("lorena:{A},luis:{B}");
        let access = Access::new(Some(&tokens), Some("http://localhost:3000"))
            .unwrap()
            .unwrap();
        assert!(!access.secure);
        assert!(access.grants(A).is_some());
        assert_eq!(access.grants(&B.to_uppercase()), Uuid::parse_str(B).ok());
        assert!(
            access
                .grants("00000000-0000-0000-0000-000000000000")
                .is_none()
        );
        assert!(access.grants("lorena").is_none());
    }

    #[test]
    fn reads_the_cookie_among_others() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "a=1; bt_token=x; bt_tokenx=y".parse().unwrap(),
        );
        assert_eq!(cookie_token(&headers), Some("x"));
        assert_eq!(cookie_token(&HeaderMap::new()), None);
    }
}
