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
    /// Builds the Access links for `tokens`, which Listeners open at `public_url`.
    ///
    /// `tokens` are each invited person's name and token, already checked with [`validate`];
    /// `public_url` is checked with [`public_url`].
    pub fn new(tokens: Vec<(String, Uuid)>, public_url: &Url) -> Self {
        Self {
            tokens,
            origin: public_url.origin().ascii_serialization(),
            secure: public_url.scheme() == "https",
        }
    }

    /// Returns each invited person's name and Access link.
    ///
    /// A link is `<origin>/?token=<uuid>`.
    pub fn links(&self) -> impl Iterator<Item = (&str, String)> {
        self.tokens
            .iter()
            .map(|(name, token)| (name.as_str(), format!("{}/?token={token}", self.origin)))
    }

    /// Checks whether `token` is a configured one.
    ///
    /// `token` is compared in constant time against every configured token, so the time taken
    /// doesn't reveal how close it came. Returns it as a UUID if it matches, `None` if it doesn't
    /// or isn't a UUID.
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

/// Parses `s` as the URL Listeners open.
///
/// `s` is just an origin like `https://bt.example.com`, with or without a trailing `/`.
///
/// # Errors
///
/// Fails if `s` isn't a URL, isn't `http(s)` or has a path, query or fragment.
pub fn public_url(s: &str) -> anyhow::Result<Url> {
    let url = Url::parse(s).context("must be a URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("must be http or https");
    }
    // The page asks for `/ws` and `/stream` at the host's root, so bt can't live under a path.
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        bail!("must not have a path, query or fragment");
    }
    Ok(url)
}

/// Parses the value of `BT_ACCESS_TOKENS`.
///
/// `s` is `name:uuid,name:uuid`, one entry per invited person; whitespace around names, UUIDs
/// and entries is ignored. Returns the entries in order, still to be checked with [`validate`];
/// none if `s` is blank.
///
/// # Errors
///
/// Fails if an entry has no name or no valid UUID. Errors never include a UUID.
pub fn parse(s: &str) -> anyhow::Result<Vec<(String, Uuid)>> {
    if s.trim().is_empty() {
        return Ok(Vec::new());
    }
    s.split(',')
        .map(str::trim)
        .enumerate()
        .map(|(i, entry)| {
            let Some((name, t)) = entry.split_once(':') else {
                bail!("entry {} must be name:uuid", i + 1);
            };
            let name = name.trim();
            if name.is_empty() {
                bail!("entry {} has no name", i + 1);
            }
            Ok((name.into(), token(name, t.trim())?))
        })
        .collect()
}

/// Parses `s` as `name`'s token.
///
/// # Errors
///
/// Fails if `s` is not a UUID; the error doesn't include it.
pub fn token(name: &str, s: &str) -> anyhow::Result<Uuid> {
    Uuid::parse_str(s).map_err(|_| anyhow::anyhow!("{name}'s token is not a UUID"))
}

/// Checks the Access link entries `tokens`, each a name and its token.
///
/// # Errors
///
/// Fails if there are no entries, an entry has a blank name, or a name or token appears twice.
/// Errors never include a UUID.
pub fn validate(tokens: &[(String, Uuid)]) -> anyhow::Result<()> {
    if tokens.is_empty() {
        bail!("no entries");
    }
    for (i, (name, token)) in tokens.iter().enumerate() {
        if name.trim().is_empty() {
            bail!("entry {} has no name", i + 1);
        }
        let before = &tokens[..i];
        if before.iter().any(|(n, _)| n == name) {
            bail!("{name} appears twice");
        }
        if before.iter().any(|(_, t)| t == token) {
            bail!("{name} reuses another entry's token");
        }
    }
    Ok(())
}

/// Lets a request through only with a valid Access link; the middleware for every route.
///
/// - `/?token=<uuid>` with a valid token sets the [`COOKIE`] cookie (`Secure` over HTTPS) and
///   redirects to `/`; with an invalid one it gets 401.
/// - Any other request without a cookie holding a valid token gets 401, with the [`DENIED`]
///   page.
/// - `/ws` must also have an `Origin` equal to the public origin, or it gets 403, since
///   `SameSite=Lax` still sends the cookie from sibling subdomains.
///
/// A request let through carries its token as a [`Uuid`] extension, to tell Listeners apart.
pub async fn check(State(access): State<Arc<Access>>, mut req: Request, next: Next) -> Response {
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
    let Some(token) = cookie_token(req.headers()).and_then(|t| access.grants(t)) else {
        return denied;
    };
    if uri.path() == "/ws"
        && req.headers().get(header::ORIGIN).map(|o| o.as_bytes()) != Some(access.origin.as_bytes())
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    req.extensions_mut().insert(token);
    next.run(req).await
}

/// Returns the decoded `token` parameter of the URL query `query`, or `None` if it has none.
fn query_token(query: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.into_owned())
}

/// Returns the value of the [`COOKIE`] cookie among the `Cookie` headers in `headers`.
///
/// Returns `None` if there is no such cookie.
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
            let checked = parse(&input).and_then(|t| validate(&t));
            assert_eq!(checked.unwrap_err().to_string(), error, "{input:?}");
        }
    }

    /// Returns `lorena`'s and `luis`'s entries, with tokens [`A`] and [`B`].
    fn tokens() -> Vec<(String, Uuid)> {
        parse(&format!("lorena:{A},luis:{B}")).unwrap()
    }

    #[test]
    fn needs_a_public_url_at_the_root() {
        for url in [
            "bt.example.com",
            "ftp://bt.example.com",
            "https://bt.example.com/bt",
            "https://bt.example.com/?x=1",
            "https://bt.example.com/#x",
        ] {
            assert!(public_url(url).is_err(), "{url}");
        }
        let access = Access::new(tokens(), &public_url("https://bt.example.com/").unwrap());
        assert!(access.secure);
        assert_eq!(
            access.links().next(),
            Some(("lorena", format!("https://bt.example.com/?token={A}")))
        );
    }

    #[test]
    fn grants_only_configured_tokens() {
        let access = Access::new(tokens(), &public_url("http://localhost:3000").unwrap());
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
