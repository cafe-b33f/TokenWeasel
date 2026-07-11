//! Header filtering for the reverse proxy.
//!
//! Implements RFC 7230 hop-by-hop header filtering for both request and
//! response directions, including dynamic exclusion of headers listed in
//! the `Connection` header.

use axum::http::{response, HeaderMap};

/// Request headers never forwarded upstream: hop-by-hop plus headers that
/// reqwest manages itself (`host`, `content-length`, `accept-encoding`).
/// `accept-encoding` is forced to `identity` so upstream responses are
/// uncompressed for reliable usage parsing.
const SKIP_REQUEST_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    // Cookies belong to TokenWeasel's dashboard origin (including the
    // authentication session) and must never cross the proxy trust boundary.
    "cookie",
    // Force uncompressed upstream responses so we can parse usage reliably.
    "accept-encoding",
];

/// Client auth headers stripped from upstream when a client `twl_` key
/// was consumed. The backend credential is injected after stripping.
const CLIENT_AUTH_HEADERS: &[&str] = &["authorization", "x-api-key"];

/// Response headers never forwarded back to the client. `content-length`
/// is dropped because the body is re-framed as a stream (chunked).
const SKIP_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    // Upstreams are outside the dashboard origin's trust boundary.
    "set-cookie",
    // `content-length` is dropped because we re-frame the body as a stream
    // (chunked); a stale length would corrupt the response.
    "content-length",
];

/// Collect header names from the `Connection` header value (RFC 7230 §6.1).
/// Returns lowercased names from the comma-separated list.
pub(crate) fn connection_header_names(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(http::header::CONNECTION)
        .iter()
        .flat_map(|val| {
            val.to_str().ok().into_iter().flat_map(|s| {
                s.split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(|t| t.to_ascii_lowercase())
                    .collect::<Vec<_>>()
            })
        })
        .collect()
}

/// Return an iterator over headers that should be forwarded (not hop-by-hop).
/// Applies the static skip list plus dynamic exclusions from the `Connection`
/// header and any `extra_skip` names.
pub(crate) fn skip_headers<'a>(
    req: bool,
    headers: &'a HeaderMap,
    extra_skip: &'a [&str],
) -> impl Iterator<Item = (&'a str, &'a http::HeaderValue)> + 'a {
    let dynamic = connection_header_names(headers);
    let list = if req {
        SKIP_REQUEST_HEADERS
    } else {
        SKIP_RESPONSE_HEADERS
    };
    headers.iter().filter_map(move |(name, value)| {
        let s = name.as_str();
        if list.contains(&s) || dynamic.contains(&s.to_ascii_lowercase()) || extra_skip.contains(&s)
        {
            None
        } else {
            Some((s, value))
        }
    })
}

/// Copy forwardable client headers onto the upstream request. Filters out
/// hop-by-hop headers and headers that reqwest manages itself.
///
/// When `strip_client_auth` is true, `Authorization` and `x-api-key` are
/// also excluded (the backend credential is injected after this returns).
pub fn forward_request_headers(
    mut req: reqwest::RequestBuilder,
    headers: &HeaderMap,
    strip_client_auth: bool,
) -> reqwest::RequestBuilder {
    let extra_skip: &[&str] = if strip_client_auth {
        CLIENT_AUTH_HEADERS
    } else {
        &[]
    };
    for (name, value) in skip_headers(true, headers, extra_skip) {
        req = req.header(name, value);
    }
    req.header(http::header::ACCEPT_ENCODING, "identity")
}

/// Copy forwardable upstream headers onto the response. Filters out hop-by-hop
/// and length headers that would corrupt a streamed response.
pub fn forward_response_headers(
    mut builder: response::Builder,
    headers: &reqwest::header::HeaderMap,
) -> response::Builder {
    let active_html = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .is_some_and(|mime| {
            mime.eq_ignore_ascii_case("text/html")
                || mime.eq_ignore_ascii_case("application/xhtml+xml")
                || mime.eq_ignore_ascii_case("image/svg+xml")
        });
    let extra_skip: &[&str] = if active_html {
        &[
            "content-disposition",
            "content-security-policy",
            "content-security-policy-report-only",
            "x-content-type-options",
        ]
    } else {
        &[]
    };

    for (name, value) in skip_headers(false, headers, extra_skip) {
        builder = builder.header(name, value);
    }
    if active_html {
        builder = builder
            .header(
                "content-security-policy",
                "sandbox; default-src 'none'; base-uri 'none'; form-action 'none'",
            )
            .header("x-content-type-options", "nosniff")
            .header("content-disposition", "attachment");
    }
    builder
}
