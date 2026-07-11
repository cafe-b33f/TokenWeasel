//! Tests for hop-by-hop header filtering (static + dynamic).

use axum::http::HeaderMap;

use crate::proxy::headers::{forward_request_headers, forward_response_headers, skip_headers};

#[test]
fn outbound_requests_explicitly_require_identity_encoding() {
    let client_headers = hm(&[("accept-encoding", "gzip, br"), ("x-custom", "value")]);
    let request = forward_request_headers(
        reqwest::Client::new().get("http://example.com/v1/chat/completions"),
        &client_headers,
        false,
    )
    .build()
    .unwrap();

    assert_eq!(
        request
            .headers()
            .get(axum::http::header::ACCEPT_ENCODING)
            .and_then(|value| value.to_str().ok()),
        Some("identity")
    );
    assert_eq!(request.headers()["x-custom"], "value");
}

#[test]
fn outbound_requests_never_forward_dashboard_or_other_cookies() {
    let client_headers = hm(&[
        ("cookie", "theme=dark; twl_session=secret-session-token"),
        ("x-custom", "value"),
    ]);
    let request = forward_request_headers(
        reqwest::Client::new().get("http://example.com/v1/chat/completions"),
        &client_headers,
        false,
    )
    .build()
    .unwrap();

    assert!(request.headers().get(axum::http::header::COOKIE).is_none());
    assert_eq!(request.headers()["x-custom"], "value");
}

/// Build a header map with the given key-value pairs.
fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (k, v) in pairs {
        map.append(
            k.parse::<axum::http::HeaderName>().unwrap(),
            v.parse().unwrap(),
        );
    }
    map
}

// Static skip list

#[test]
fn static_hop_by_hop_dropped_from_request() {
    let headers = hm(&[
        ("host", "example.com"),
        ("content-length", "0"),
        ("connection", "keep-alive"),
        ("keep-alive", "timeout=5"),
        ("x-custom", "value"),
    ]);
    let counted: Vec<_> = skip_headers(true, &headers, &[]).collect();
    let names: Vec<&str> = counted.into_iter().map(|(n, _)| n).collect();
    assert!(!names.contains(&"host"));
    assert!(!names.contains(&"content-length"));
    assert!(!names.contains(&"connection"));
    assert!(!names.contains(&"keep-alive"));
    assert!(names.contains(&"x-custom"));
}

#[test]
fn static_hop_by_hop_dropped_from_response() {
    let headers = hm(&[
        ("connection", "keep-alive"),
        ("transfer-encoding", "chunked"),
        ("x-forwarded-for", "1.2.3.4"),
    ]);
    let counted: Vec<_> = skip_headers(false, &headers, &[]).collect();
    let names: Vec<&str> = counted.into_iter().map(|(n, _)| n).collect();
    assert!(!names.contains(&"connection"));
    assert!(!names.contains(&"transfer-encoding"));
    assert!(names.contains(&"x-forwarded-for"));
}

#[test]
fn active_upstream_response_cannot_set_cookies_or_relax_browser_sandbox() {
    for content_type in [
        "Text/HTML; charset=utf-8",
        "application/xhtml+xml",
        "image/svg+xml",
    ] {
        let headers = hm(&[
            ("content-type", content_type),
            ("set-cookie", "twl_session=attacker; Path=/; HttpOnly"),
            ("content-security-policy", "script-src * 'unsafe-inline'"),
            ("content-disposition", "inline"),
            ("x-upstream", "preserved"),
        ]);
        let response = forward_response_headers(http::Response::builder(), &headers)
            .body(())
            .unwrap();

        assert!(response.headers().get(http::header::SET_COOKIE).is_none());
        assert_eq!(
            response.headers()["content-security-policy"],
            "sandbox; default-src 'none'; base-uri 'none'; form-action 'none'"
        );
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["content-disposition"], "attachment");
        assert_eq!(response.headers()["x-upstream"], "preserved");
    }
}

#[test]
fn api_stream_headers_remain_forwardable_under_response_sandbox() {
    let headers = hm(&[
        ("content-type", "text/event-stream"),
        ("content-disposition", "attachment; filename=events.log"),
        ("content-security-policy", "default-src 'self'"),
        ("x-stream", "yes"),
    ]);
    let response = forward_response_headers(http::Response::builder(), &headers)
        .body(())
        .unwrap();

    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(
        response.headers()["content-disposition"],
        "attachment; filename=events.log"
    );
    assert_eq!(response.headers()["x-stream"], "yes");
    assert_eq!(
        response.headers()["content-security-policy"],
        "default-src 'self'"
    );
}

// Dynamic Connection header filtering

#[test]
fn dynamic_connection_header_names_parsed() {
    use crate::proxy::headers::connection_header_names;

    let headers = hm(&[
        ("x-middleware", "value1"),
        ("connection", "x-middleware, x-custom"),
        ("x-custom", "value2"),
    ]);
    let dynamic = connection_header_names(&headers);
    assert!(dynamic.contains(&"x-middleware".to_string()));
    assert!(dynamic.contains(&"x-custom".to_string()));
    assert!(!dynamic.contains(&"connection".to_string())); // not self-referential

    // Whitespace around names is trimmed
    let headers2 = hm(&[("connection", " a , b , c ")]);
    let dynamic2 = connection_header_names(&headers2);
    assert_eq!(dynamic2, vec!["a", "b", "c"]);
}

#[test]
fn dynamic_connection_headers_filtered_from_request() {
    let headers = hm(&[
        ("host", "example.com"),
        ("x-middleware", "value1"),
        ("connection", "x-middleware, x-custom"),
        ("x-custom", "value2"),
        ("x-allowed", "value3"),
    ]);
    let counted: Vec<_> = skip_headers(true, &headers, &[]).collect();
    let names: Vec<&str> = counted.into_iter().map(|(n, _)| n).collect();
    // Static hop-by-hop
    assert!(!names.contains(&"host"));
    // Dynamic hop-by-hop (from Connection header)
    assert!(!names.contains(&"x-middleware"));
    assert!(!names.contains(&"x-custom"));
    // Allowed header passes through
    assert!(names.contains(&"x-allowed"));
}

#[test]
fn dynamic_connection_headers_filtered_from_response() {
    let headers = hm(&[
        ("connection", "proxy-auth, x-gateway"),
        ("proxy-auth", "scheme realm"),
        ("x-gateway", "gw1"),
        ("etag", "\"abc\""),
    ]);
    let counted: Vec<_> = skip_headers(false, &headers, &[]).collect();
    let names: Vec<&str> = counted.into_iter().map(|(n, _)| n).collect();
    assert!(!names.contains(&"proxy-auth")); // dynamic from Connection
    assert!(!names.contains(&"x-gateway")); // dynamic from Connection
    assert!(names.contains(&"etag"));
}

#[test]
fn case_insensitive_dynamic_matching() {
    // HeaderName normalizes keys to lowercase, so the map key is "x-middleware"
    // regardless of how it was set. The Connection header value "x-MIDDLEWARE"
    // is also lowercased by connection_header_names(). This test verifies that
    // the dynamic filter matches the normalized key.
    let headers = hm(&[("X-Middleware", "value"), ("Connection", "x-MIDDLEWARE")]);
    let counted: Vec<_> = skip_headers(true, &headers, &[]).collect();
    let names: Vec<&str> = counted.into_iter().map(|(n, _)| n).collect();
    // "x-middleware" must be absent (lowercase, matching HeaderName's normalization)
    assert!(!names.contains(&"x-middleware"));
}

#[test]
fn empty_connection_header_is_harmless() {
    let headers = hm(&[("connection", ""), ("x-header", "value")]);
    let counted: Vec<_> = skip_headers(true, &headers, &[]).collect();
    let names: Vec<&str> = counted.into_iter().map(|(n, _)| n).collect();
    assert!(names.contains(&"x-header"));
}

#[test]
fn no_connection_header_means_no_dynamic_filters() {
    let headers = hm(&[("host", "example.com"), ("x-header", "value")]);
    let counted: Vec<_> = skip_headers(true, &headers, &[]).collect();
    let names: Vec<&str> = counted.into_iter().map(|(n, _)| n).collect();
    assert!(!names.contains(&"host"));
    assert!(names.contains(&"x-header"));
}
