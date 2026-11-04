//! REST + MCP tests: the router is driven in-process with `oneshot`; MemRepo, FsBlobStore on a tempdir, no LLM, no network.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use marginalia::api;
use marginalia::blob::FsBlobStore;
use marginalia::mcp::MarginaliaMcp;
use marginalia::mem::MemRepo;
use marginalia::service::{App, AppConfig};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ClientConfig};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt as _;

const TOKEN: &str = "s3cret";

fn token_auth() -> api::Auth {
    api::Auth { token: Some(TOKEN.into()), tailscale_logins: vec![] }
}

/// The router as served on a Unix socket: the token plus the owner's Tailscale login.
fn tailscale_auth() -> api::Auth {
    api::Auth { token: Some(TOKEN.into()), tailscale_logins: vec!["Owner@GitHub".into(), " other@github ".into()] }
}

fn app() -> (Arc<App>, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let app = App::new(Arc::new(MemRepo::new()), Arc::new(FsBlobStore::new(dir.path().join("blobs"))), AppConfig::default());
    (Arc::new(app), dir)
}

async fn call(router: &Router, method: &str, uri: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let body = match body {
        Some(v) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = router.clone().oneshot(req.body(body).expect("request")).await.expect("response");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    let value = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
    (status, value)
}

async fn save_sample(router: &Router) -> String {
    let (status, article) = call(
        router,
        "POST",
        "/articles",
        Some(TOKEN),
        Some(json!({ "url": "https://example.com/otters", "title": "Otters at play", "markdown": "Otters hold hands while they sleep so they do not drift apart." })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{article}");
    article["id"].as_str().expect("id").to_string()
}

#[tokio::test]
async fn bearer_token_is_enforced_except_health() {
    let (app, _dir) = app();
    let router = api::router(app, token_auth());

    let (status, body) = call(&router, "GET", "/articles", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].is_string());
    assert_eq!(call(&router, "GET", "/articles", Some("wrong"), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&router, "GET", "/articles", Some("s3cre"), None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call(&router, "GET", "/articles", Some(TOKEN), None).await, (StatusCode::OK, json!([])));

    let (status, health) = call(&router, "GET", "/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health, json!({ "ok": true, "llm": false }));

    // MCP sits behind the same middleware.
    assert_eq!(call(&router, "POST", "/mcp", None, Some(json!({}))).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn no_token_means_open() {
    let (app, _dir) = app();
    let router = api::router(app, api::Auth::default());
    assert_eq!(call(&router, "GET", "/articles", None, None).await.0, StatusCode::OK);
}

#[tokio::test]
async fn article_lifecycle() {
    let (app, _dir) = app();
    let router = api::router(app, token_auth());
    let id = save_sample(&router).await;

    let (status, article) = call(&router, "GET", &format!("/articles/{id}"), Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(article["title"], "Otters at play");
    assert_eq!(article["via"], "manual");

    let (_, list) = call(&router, "GET", "/articles?limit=10&offset=0", Some(TOKEN), None).await;
    assert_eq!(list.as_array().map(Vec::len), Some(1));
    assert_eq!(list[0]["id"], id.as_str());
    assert!(list[0].get("markdown").is_none());
    assert_eq!(call(&router, "GET", "/articles?offset=1", Some(TOKEN), None).await.1, json!([]));

    let (status, hl) = call(&router, "POST", &format!("/articles/{id}/highlights"), Some(TOKEN), Some(json!({ "text": "hold hands", "note": "cute" }))).await;
    assert_eq!(status, StatusCode::CREATED, "{hl}");
    assert_eq!(hl["note"], "cute");
    let (_, per_article) = call(&router, "GET", &format!("/articles/{id}/highlights"), Some(TOKEN), None).await;
    assert_eq!(per_article.as_array().map(Vec::len), Some(1));
    let (_, all) = call(&router, "GET", "/highlights", Some(TOKEN), None).await;
    assert_eq!(all.as_array().map(Vec::len), Some(1));

    let (status, hits) = call(&router, "GET", "/search?q=otters&limit=5", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hits[0]["article_id"], id.as_str());
    assert_eq!(call(&router, "GET", "/search?q=", Some(TOKEN), None).await.1, json!([]));

    assert_eq!(call(&router, "DELETE", &format!("/articles/{id}"), Some(TOKEN), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&router, "GET", &format!("/articles/{id}"), Some(TOKEN), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&router, "GET", "/highlights", Some(TOKEN), None).await.1, json!([]));
}

#[tokio::test]
async fn errors_map_to_statuses() {
    let (app, _dir) = app();
    let router = api::router(app, token_auth());

    let (status, body) = call(&router, "GET", "/articles/nope", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().is_some_and(|m| m.contains("nope")));
    assert_eq!(call(&router, "DELETE", "/articles/nope", Some(TOKEN), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&router, "GET", "/syntheses/nope", Some(TOKEN), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        call(&router, "POST", "/articles/nope/highlights", Some(TOKEN), Some(json!({ "text": "x" }))).await.0,
        StatusCode::NOT_FOUND
    );

    // 400: bad input of every kind, always with an {error} body.
    let (status, body) = call(&router, "POST", "/articles", Some(TOKEN), Some(json!({ "url": "https://x.test", "title": "t" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].is_string());
    let (status, _) = call(&router, "POST", "/articles", Some(TOKEN), Some(json!({ "url": "ftp://x.test" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&router, "POST", "/articles", Some(TOKEN), Some(json!({ "nope": 1 }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&router, "GET", "/syntheses?kind=poem", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&router, "GET", "/articles?limit=abc", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // 503: everything that needs an LLM.
    let (status, body) = call(&router, "POST", "/chat", Some(TOKEN), Some(json!({ "message": "hi" }))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body["error"].is_string());
    assert_eq!(call(&router, "POST", "/briefs", Some(TOKEN), Some(json!({ "direction": "risks" }))).await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(call(&router, "POST", "/briefs", Some(TOKEN), None).await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(call(&router, "POST", "/articles/x/analyse", Some(TOKEN), None).await.0, StatusCode::SERVICE_UNAVAILABLE);

    assert_eq!(call(&router, "DELETE", "/chat/some-session", Some(TOKEN), None).await.0, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn syntheses_are_listed_and_filtered() {
    let (app, _dir) = app();
    let saved = app
        .repo()
        .save_synthesis(marginalia::domain::NewSynthesis {
            kind: marginalia::domain::SynthesisKind::Brief,
            title: "Brief".into(),
            direction: None,
            body: "body".into(),
            article_ids: vec![],
        })
        .await
        .expect("save");
    let router = api::router(app, api::Auth::default());
    let (_, all) = call(&router, "GET", "/syntheses", None, None).await;
    assert_eq!(all.as_array().map(Vec::len), Some(1));
    assert_eq!(call(&router, "GET", "/syntheses?kind=analysis", None, None).await.1, json!([]));
    assert_eq!(call(&router, "GET", "/syntheses?kind=brief&limit=1", None, None).await.1.as_array().map(Vec::len), Some(1));
    let (status, one) = call(&router, "GET", &format!("/syntheses/{}", saved.id), None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["title"], "Brief");
}

// ---------- MCP ----------

fn args(v: Value) -> serde_json::Map<String, Value> {
    v.as_object().cloned().expect("object")
}

fn text_of(result: &rmcp::model::CallToolResult) -> String {
    serde_json::to_value(&result.content).expect("content").pointer("/0/text").and_then(Value::as_str).unwrap_or_default().to_string()
}

#[tokio::test]
async fn mcp_tools_over_in_process_transport() {
    let (app, _dir) = app();
    let (server_io, client_io) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(async move {
        let running = MarginaliaMcp::new(app).serve(server_io).await.expect("serve");
        let _ = running.waiting().await;
    });
    let client = ClientConfig::default().serve(client_io).await.expect("client");

    let info = client.peer_info().expect("peer info");
    assert!(info.instructions.as_deref().is_some_and(|i| i.contains("search")));

    let tools = client.list_all_tools().await.expect("tools");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    for expected in [
        "list_articles", "get_article", "search", "list_highlights", "add_highlight", "save_url", "save_markdown",
        "list_syntheses", "get_synthesis", "save_synthesis", "analyse_article", "write_brief",
    ] {
        assert!(names.contains(&expected), "missing tool {expected}: {names:?}");
    }
    assert!(!names.iter().any(|n| n.contains("delete")));

    let saved = client
        .call_tool(CallToolRequestParams::new("save_markdown").with_arguments(args(
            json!({ "url": "https://example.com/otters", "title": "Otters at play", "markdown": "Otters hold hands while they sleep." }),
        )))
        .await
        .expect("save_markdown");
    assert_ne!(saved.is_error, Some(true));
    let id = serde_json::from_str::<Value>(&text_of(&saved)).expect("json")["id"].as_str().expect("id").to_string();

    let found = client.call_tool(CallToolRequestParams::new("search").with_arguments(args(json!({ "query": "otters" })))).await.expect("search");
    assert_ne!(found.is_error, Some(true));
    let hits: Value = serde_json::from_str(&text_of(&found)).expect("json");
    assert_eq!(hits[0]["article_id"], id.as_str());

    let listed = client.call_tool(CallToolRequestParams::new("list_articles").with_arguments(args(json!({})))).await.expect("list");
    assert_eq!(serde_json::from_str::<Value>(&text_of(&listed)).expect("json").as_array().map(Vec::len), Some(1));

    let hl = client
        .call_tool(CallToolRequestParams::new("add_highlight").with_arguments(args(json!({ "article_id": id, "text": "hold hands" }))))
        .await
        .expect("add_highlight");
    assert_ne!(hl.is_error, Some(true));

    let synth = client
        .call_tool(CallToolRequestParams::new("save_synthesis").with_arguments(args(
            json!({ "kind": "brief", "title": "Otters", "body": "They hold hands.", "article_ids": [id] }),
        )))
        .await
        .expect("save_synthesis");
    assert_ne!(synth.is_error, Some(true));
    let listed = client.call_tool(CallToolRequestParams::new("list_syntheses").with_arguments(args(json!({ "kind": "brief" })))).await.expect("list_syntheses");
    assert_eq!(serde_json::from_str::<Value>(&text_of(&listed)).expect("json").as_array().map(Vec::len), Some(1));

    // Failures are tool errors, not protocol errors.
    for (tool, a) in [
        ("get_article", json!({ "id": "missing" })),
        ("add_highlight", json!({ "article_id": "missing", "text": "x" })),
        ("save_synthesis", json!({ "kind": "poem", "title": "t", "body": "b", "article_ids": [] })),
        ("write_brief", json!({})),
    ] {
        let res = client.call_tool(CallToolRequestParams::new(tool).with_arguments(args(a))).await.unwrap_or_else(|e| panic!("{tool} was a protocol error: {e}"));
        assert_eq!(res.is_error, Some(true), "{tool}");
        assert!(!text_of(&res).is_empty());
    }

    let _ = client.cancel().await;
    let _ = server.await;
}

#[tokio::test]
async fn export_needs_the_token_and_returns_the_whole_library() {
    let (app, _dir) = app();
    let router = api::router(app, token_auth());

    let (status, body) = call(&router, "GET", "/export", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body["error"].is_string());
    assert_eq!(call(&router, "GET", "/export", Some("wrong"), None).await.0, StatusCode::UNAUTHORIZED);

    let (status, empty) = call(&router, "GET", "/export", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty["version"], 1);
    assert_eq!((empty["articles"].clone(), empty["highlights"].clone(), empty["syntheses"].clone()), (json!([]), json!([]), json!([])));

    let id = save_sample(&router).await;
    let (status, _) = call(&router, "POST", &format!("/articles/{id}/highlights"), Some(TOKEN), Some(json!({ "text": "hold hands" }))).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, export) = call(&router, "GET", "/export", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(export["version"], 1);
    assert!(export["exported_at"].is_string());
    assert_eq!(export["articles"][0]["id"], id.as_str());
    assert_eq!(export["articles"][0]["markdown"], "Otters hold hands while they sleep so they do not drift apart.");
    assert_eq!(export["highlights"][0]["article_id"], id.as_str());
    assert_eq!(export["syntheses"], json!([]));

    // Import is CLI-only: there is no REST route for it.
    for method in ["POST", "PUT"] {
        let (status, _) = call(&router, method, "/export", Some(TOKEN), Some(export.clone())).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }
    assert_eq!(call(&router, "POST", "/import", Some(TOKEN), Some(export)).await.0, StatusCode::NOT_FOUND);
}

// ---------- CORS for the Tauri app ----------

async fn with_origin(router: &Router, method: &str, uri: &str, origin: &str, extra: &[(&str, &str)]) -> axum::http::Response<Body> {
    let mut req = Request::builder().method(method).uri(uri).header(header::ORIGIN, origin);
    for (k, v) in extra {
        req = req.header(*k, *v);
    }
    router.clone().oneshot(req.body(Body::empty()).expect("request")).await.expect("response")
}

fn hv<'a>(res: &'a axum::http::Response<Body>, name: &str) -> Option<&'a str> {
    res.headers().get(name).and_then(|v| v.to_str().ok())
}

#[tokio::test]
async fn cors_allows_the_app_origins_to_preflight_without_a_token() {
    let (app, _dir) = app();
    let router = api::router(app, token_auth());

    for origin in ["tauri://localhost", "http://tauri.localhost", "https://tauri.localhost"] {
        for method in ["GET", "POST", "DELETE"] {
            let res = with_origin(
                &router,
                "OPTIONS",
                "/articles",
                origin,
                &[("access-control-request-method", method), ("access-control-request-headers", "authorization,content-type")],
            )
            .await;
            assert!(res.status().is_success(), "{origin} {method}: {}", res.status());
            assert_eq!(hv(&res, "access-control-allow-origin"), Some(origin));
            let headers = hv(&res, "access-control-allow-headers").unwrap_or("").to_ascii_lowercase();
            assert!(headers.contains("authorization") && headers.contains("content-type"), "{headers}");
            assert!(hv(&res, "access-control-allow-methods").is_some_and(|m| m.contains(method)));
            // Bearer headers, never cookies: credentials are not allowed.
            assert_eq!(hv(&res, "access-control-allow-credentials"), None);
        }
    }
}

#[tokio::test]
async fn cors_answers_real_requests_for_the_app_including_401() {
    let (app, _dir) = app();
    let router = api::router(app, token_auth());
    let origin = "tauri://localhost";

    let res = with_origin(&router, "GET", "/health", origin, &[]).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(hv(&res, "access-control-allow-origin"), Some(origin));

    let res = with_origin(&router, "GET", "/articles", origin, &[("authorization", "Bearer s3cret")]).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(hv(&res, "access-control-allow-origin"), Some(origin));
    assert_eq!(hv(&res, "access-control-allow-credentials"), None);

    // A rejected token must still be readable by the app, or it could not tell "wrong token" from "network error".
    let res = with_origin(&router, "GET", "/articles", origin, &[("authorization", "Bearer nope")]).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(hv(&res, "access-control-allow-origin"), Some(origin));
}

#[tokio::test]
async fn cors_refuses_every_other_origin() {
    let (app, _dir) = app();
    let router = api::router(app, token_auth());

    for origin in [
        "https://evil.example",
        "http://localhost",
        "http://localhost:1420",
        "tauri://localhost.evil.example",
        "null",
        "https://marginalia-bytetum.taild98079.ts.net",
    ] {
        let pre = with_origin(
            &router,
            "OPTIONS",
            "/articles",
            origin,
            &[("access-control-request-method", "GET"), ("access-control-request-headers", "authorization")],
        )
        .await;
        assert_eq!(hv(&pre, "access-control-allow-origin"), None, "preflight from {origin}");
        let res = with_origin(&router, "GET", "/articles", origin, &[("authorization", "Bearer s3cret")]).await;
        assert_eq!(hv(&res, "access-control-allow-origin"), None, "{origin}");
    }
    // No Origin header (curl, the browser page on its own origin): no CORS headers at all.
    let (_, _) = call(&router, "GET", "/health", None, None).await;
    let req = Request::builder().method("GET").uri("/health").body(Body::empty()).expect("request");
    let res = router.clone().oneshot(req).await.expect("response");
    assert_eq!(hv(&res, "access-control-allow-origin"), None);
}

/// `call` with arbitrary request headers and no body.
async fn call_with(router: &Router, method: &str, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = router.clone().oneshot(req.body(Body::empty()).expect("request")).await.expect("response");
    let status = res.status();
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

const LOGIN: &str = "tailscale-user-login";

#[tokio::test]
async fn trusted_tailscale_login_needs_no_token() {
    let (app, _dir) = app();
    let router = api::router(app, tailscale_auth());

    for login in ["owner@github", "OWNER@github", "  owner@GITHUB ", "other@github"] {
        let (status, body) = call_with(&router, "GET", "/articles", &[(LOGIN, login)]).await;
        assert_eq!((status, body), (StatusCode::OK, json!([])), "{login:?}");
    }
    // MCP sits behind the same check: it gets past auth (what the MCP layer answers to an empty body is not our business).
    let (status, _) = call_with(&router, "POST", "/mcp", &[(LOGIN, "owner@github")]).await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);

    // Anyone else, a partial match, an empty value, or a token-less request stays out.
    for login in ["intruder@github", "owner@githu", "owner@github.evil", "", "owner@github,other@github"] {
        let (status, _) = call_with(&router, "GET", "/articles", &[(LOGIN, login)]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{login:?}");
    }
    assert_eq!(call(&router, "GET", "/articles", None, None).await.0, StatusCode::UNAUTHORIZED);
    // Two copies of the header are ambiguous, so neither is trusted.
    let (status, _) = call_with(&router, "GET", "/articles", &[(LOGIN, "owner@github"), (LOGIN, "owner@github")]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The bearer token keeps working next to it, and a wrong one is not rescued by nothing.
    assert_eq!(call(&router, "GET", "/articles", Some(TOKEN), None).await.0, StatusCode::OK);
    assert_eq!(call(&router, "GET", "/articles", Some("wrong"), None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn tailscale_header_is_ignored_when_no_logins_are_configured() {
    // What the TCP listener builds: any local account can send this header there, so it must count for nothing.
    let (app, _dir) = app();
    let router = api::router(app, token_auth());
    let (status, _) = call_with(&router, "GET", "/articles", &[(LOGIN, "owner@github")]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call_with(&router, "GET", "/me", &[(LOGIN, "owner@github")]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = call_with(&router, "GET", "/save?url=https://example.com", &[(LOGIN, "owner@github")]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Blank logins never match an empty header either.
    let (blank_app, _dir2) = self::app();
    let blank = api::Auth { token: Some(TOKEN.into()), tailscale_logins: vec!["".into(), "  ".into()] };
    let router = api::router(blank_app, blank);
    assert_eq!(call_with(&router, "GET", "/articles", &[(LOGIN, "")]).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn save_link_accepts_a_trusted_login_instead_of_the_token() {
    let (app, _dir) = app();
    let router = api::router(app, tailscale_auth());
    // Past auth means the missing `url` is what it complains about (400); no LLM or network is needed.
    assert_eq!(call_with(&router, "GET", "/save", &[(LOGIN, "owner@github")]).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call_with(&router, "GET", "/save", &[(LOGIN, "intruder@github")]).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call_with(&router, "GET", "/save", &[]).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(call_with(&router, "GET", "/save?token=s3cret", &[]).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn save_link_from_another_site_asks_before_saving_with_the_login_alone() {
    // An <img src=/save?url=...> on any page the owner opens would otherwise save that URL and make the server fetch
    // it. The login is ambient; the browser's Sec-Fetch-Site label tells the two apart. No network is needed: the
    // refused requests never fetch, and the allowed ones lack a url.
    let (app, _dir) = app();
    let router = api::router(app, tailscale_auth());
    for site in ["cross-site", "same-site"] {
        let (status, headers, body) = raw(
            &router,
            "GET",
            "/save?url=https%3A%2F%2Fevil.example%2F%22%3E%3Cscript%3E",
            &[(LOGIN, "owner@github"), ("sec-fetch-site", site)],
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{site}");
        let page = String::from_utf8_lossy(&body);
        assert!(page.contains("Save this page?") && page.contains("action=\"/save\""), "{page}");
        assert!(page.contains("https://evil.example/&quot;&gt;&lt;script&gt;") && !page.contains("<script>"), "{page}");
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    }
    let (_, body) = call(&router, "GET", "/articles", Some(TOKEN), None).await;
    assert_eq!(body.as_array().map(Vec::len), Some(0), "nothing was saved");

    // The confirm form's own submit (same-origin), a typed URL or share sheet (none), and non-browser clients (no
    // header) get past auth: the missing url is what they hit.
    for site in [Some("same-origin"), Some("none"), None] {
        let mut h = vec![(LOGIN, "owner@github")];
        if let Some(site) = site {
            h.push(("sec-fetch-site", site));
        }
        assert_eq!(call_with(&router, "GET", "/save", &h).await.0, StatusCode::BAD_REQUEST, "{site:?}");
    }
    // The token still works from anywhere: a bookmarklet runs on the other site's page.
    let h = [("sec-fetch-site", "cross-site")];
    assert_eq!(call_with(&router, "GET", "/save?token=s3cret", &h).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn me_reports_how_the_caller_got_in() {
    let (app, _dir) = app();
    let router = api::router(app, tailscale_auth());
    assert_eq!(call(&router, "GET", "/me", None, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        call(&router, "GET", "/me", Some(TOKEN), None).await,
        (StatusCode::OK, json!({ "auth": "token", "login": null }))
    );
    assert_eq!(
        call_with(&router, "GET", "/me", &[(LOGIN, " owner@github ")]).await,
        (StatusCode::OK, json!({ "auth": "tailscale", "login": "owner@github" }))
    );
    // A valid token wins over a login header.
    let both = [("authorization", "Bearer s3cret"), (LOGIN, "owner@github")];
    assert_eq!(
        call_with(&router, "GET", "/me", &both).await,
        (StatusCode::OK, json!({ "auth": "token", "login": null }))
    );

    let (open_app, _dir2) = self::app();
    let open = api::router(open_app, api::Auth::default());
    assert_eq!(call(&open, "GET", "/me", None, None).await, (StatusCode::OK, json!({ "auth": "none", "login": null })));
}

#[tokio::test]
async fn tailscale_login_cannot_be_used_by_cross_site_writes() {
    let (app, _dir) = app();
    let router = api::router(app, tailscale_auth());
    let site = |v| [(LOGIN, "owner@github"), ("sec-fetch-site", v)];
    // A page elsewhere could make the owner's browser send the header, so it cannot write...
    for v in ["cross-site", "same-site"] {
        assert_eq!(call_with(&router, "POST", "/briefs", &site(v)).await.0, StatusCode::FORBIDDEN, "{v}");
        assert_eq!(call_with(&router, "DELETE", "/articles/x", &site(v)).await.0, StatusCode::FORBIDDEN, "{v}");
    }
    // ...but the page itself, the app's webview, and non-browser clients (no Sec-Fetch-Site) can.
    assert_ne!(call_with(&router, "POST", "/briefs", &site("same-origin")).await.0, StatusCode::FORBIDDEN);
    assert_ne!(call_with(&router, "POST", "/briefs", &site("none")).await.0, StatusCode::FORBIDDEN);
    assert_ne!(call_with(&router, "POST", "/briefs", &[(LOGIN, "owner@github")]).await.0, StatusCode::FORBIDDEN);
    let app_origin = [(LOGIN, "owner@github"), ("sec-fetch-site", "cross-site"), ("origin", "tauri://localhost")];
    assert_ne!(call_with(&router, "POST", "/briefs", &app_origin).await.0, StatusCode::FORBIDDEN);
    // Cross-site reads are not blocked here (CORS stops the page reading the answer) and the token is never ambient.
    assert_eq!(call_with(&router, "GET", "/articles", &site("cross-site")).await.0, StatusCode::OK);
    let token = [("authorization", "Bearer s3cret"), ("sec-fetch-site", "cross-site")];
    assert_ne!(call_with(&router, "POST", "/briefs", &token).await.0, StatusCode::FORBIDDEN);
}

// ---------- books (EPUB) ----------

/// A tiny valid EPUB3 built here: one chapter per entry of `chapters` (title, paragraphs), titled by the nav.
fn epub(title: &str, chapters: &[(&str, Vec<String>)]) -> Vec<u8> {
    use std::io::Write;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let mut add = |name: &str, content: &str| {
        zip.start_file(name, zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored)).expect("entry");
        zip.write_all(content.as_bytes()).expect("write");
    };
    add("mimetype", "application/epub+zip");
    add(
        "META-INF/container.xml",
        r#"<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#,
    );
    let manifest: String = (0..chapters.len()).map(|i| format!(r#"<item id="c{i}" href="c{i}.xhtml" media-type="application/xhtml+xml"/>"#)).collect();
    let spine: String = (0..chapters.len()).map(|i| format!(r#"<itemref idref="c{i}"/>"#)).collect();
    add(
        "content.opf",
        &format!(
            r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>{title}</dc:title><dc:creator>Test Author</dc:creator></metadata><manifest><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>{manifest}</manifest><spine>{spine}</spine></package>"#
        ),
    );
    let links: String = chapters.iter().enumerate().map(|(i, (t, _))| format!(r#"<li><a href="c{i}.xhtml">{t}</a></li>"#)).collect();
    add(
        "nav.xhtml",
        &format!(r#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body><nav epub:type="toc"><ol>{links}</ol></nav></body></html>"#),
    );
    for (i, (_, paras)) in chapters.iter().enumerate() {
        let body: String = paras.iter().map(|p| format!("<p>{p}</p>")).collect();
        add(&format!("c{i}.xhtml"), &format!(r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>{body}</body></html>"#));
    }
    zip.finish().expect("finish").into_inner()
}

fn small_book() -> Vec<u8> {
    epub(
        "Otter Handbook",
        &[("Sleeping", vec!["Otters hold hands while they sleep.".into()]), ("Eating", vec!["Otters crack clams on their bellies.".into()])],
    )
}

/// Any request with a raw body and arbitrary headers; returns status, headers and the body bytes.
async fn raw(router: &Router, method: &str, uri: &str, headers: &[(&str, &str)], body: Vec<u8>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = router.clone().oneshot(req.body(Body::from(body)).expect("request")).await.expect("response");
    let (parts, body) = res.into_parts();
    (parts.status, parts.headers, body.collect().await.expect("body").to_bytes().to_vec())
}

fn auth_header() -> String {
    format!("Bearer {TOKEN}")
}

async fn upload(router: &Router, bytes: Vec<u8>) -> (StatusCode, Value) {
    let auth = auth_header();
    let headers = [("authorization", auth.as_str()), ("content-type", "application/epub+zip"), ("x-filename", "Otter%20Handbook.epub")];
    let (status, _, body) = raw(router, "POST", "/books", &headers, bytes).await;
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn uploading_a_book_creates_dedups_serves_the_file_and_rejects_non_epubs() {
    let (app, _dir) = app();
    let router = api::router(app, token_auth());
    let bytes = small_book();

    let (status, book) = upload(&router, bytes.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{book}");
    assert_eq!((book["kind"].as_str(), book["author"].as_str(), book["via"].as_str()), (Some("book"), Some("Test Author"), Some("epub")));
    assert_eq!(book["title"], "Otter Handbook");
    assert!(book["url"].as_str().is_some_and(|u| u.starts_with("urn:sha256:") && u.len() == "urn:sha256:".len() + 64));
    assert_eq!((book["highlight_count"].clone(), book["progress"].clone()), (json!(0), Value::Null));
    let id = book["id"].as_str().expect("id").to_string();

    // The same bytes again: 200 and the same book, not a second one.
    let (status, again) = upload(&router, bytes.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["id"], id.as_str());
    let (_, listed) = call(&router, "GET", "/articles", Some(TOKEN), None).await;
    assert_eq!(listed.as_array().map(Vec::len), Some(1));
    assert_eq!(listed[0]["kind"], "book");

    // The article carries the book text in reading order, chapter titles from the nav.
    let (_, article) = call(&router, "GET", &format!("/articles/{id}"), Some(TOKEN), None).await;
    assert_eq!(article["kind"], "book");
    assert_eq!(article["blob_key"].as_str().map(|k| k.ends_with(".epub")), Some(true));
    let md = article["markdown"].as_str().expect("markdown");
    assert!(md.starts_with("## Sleeping\n\nOtters hold hands while they sleep."), "{md}");
    assert!(md.contains("## Eating\n\nOtters crack clams"), "{md}");

    // The page asks for the metadata only, and a book has no rendered-HTML view (it is read from its EPUB).
    let (_, meta) = call(&router, "GET", &format!("/articles/{id}?body=false"), Some(TOKEN), None).await;
    assert_eq!((meta["kind"].as_str(), meta["markdown"].as_str()), (Some("book"), Some("")));
    assert_eq!(call(&router, "GET", &format!("/articles/{id}/html"), Some(TOKEN), None).await.0, StatusCode::NOT_FOUND);

    // The original bytes come back, cacheable forever, behind the token.
    let uri = format!("/articles/{id}/file");
    assert_eq!(raw(&router, "GET", &uri, &[], vec![]).await.0, StatusCode::UNAUTHORIZED);
    let auth = auth_header();
    let (status, headers, body) = raw(&router, "GET", &uri, &[("authorization", &auth)], vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, bytes);
    assert_eq!(headers[header::CONTENT_TYPE], "application/epub+zip");
    assert_eq!(headers[header::CACHE_CONTROL], "private, max-age=31536000, immutable");

    // Not an EPUB: 400 with a JSON error. Also an unauthenticated upload and a web article's /file.
    for junk in [b"hello, not a zip".to_vec(), Vec::new(), b"PK\x03\x04 truncated".to_vec()] {
        let (status, body) = upload(&router, junk).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].as_str().is_some_and(|e| e.contains("EPUB")), "{body}");
    }
    assert_eq!(raw(&router, "POST", "/books", &[("content-type", "application/epub+zip")], bytes).await.0, StatusCode::UNAUTHORIZED);
    let web = save_sample(&router).await;
    assert_eq!(call(&router, "GET", &format!("/articles/{web}/file"), Some(TOKEN), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&router, "GET", "/articles/nope/file", Some(TOKEN), None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn only_book_uploads_may_exceed_the_default_body_limit() {
    let (app, _dir) = app();
    let router = api::router(app, token_auth());
    // Over axum's 2 MiB default, so this only passes with the per-route limit.
    let big = epub("Big", &[("Part", vec!["word ".repeat(100).trim().to_string(); 6000])]);
    assert!(big.len() > 2 * 1024 * 1024, "{}", big.len());
    assert_eq!(upload(&router, big.clone()).await.0, StatusCode::CREATED);

    let auth = auth_header();
    let (status, _, _) = raw(&router, "POST", "/articles", &[("authorization", &auth), ("content-type", "application/json")], big).await;
    // The JSON extractor reports the body-limit rejection as its usual 400.
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::PAYLOAD_TOO_LARGE, "{status}");
}

#[tokio::test]
async fn progress_and_anchored_highlights_roundtrip_and_go_with_the_book() {
    let (app, _dir) = app();
    let router = api::router(app, token_auth());
    let (_, book) = upload(&router, small_book()).await;
    let id = book["id"].as_str().expect("id").to_string();
    let uri = format!("/articles/{id}/progress");

    assert_eq!(call(&router, "GET", &uri, Some(TOKEN), None).await, (StatusCode::OK, Value::Null));
    assert_eq!(call(&router, "GET", &uri, None, None).await.0, StatusCode::UNAUTHORIZED);

    let (status, put) = call(&router, "PUT", &uri, Some(TOKEN), Some(json!({ "position": "epubcfi(/6/4!/4/2/1:7)", "fraction": 0.42 }))).await;
    assert_eq!(status, StatusCode::OK, "{put}");
    assert_eq!((put["article_id"].as_str(), put["position"].as_str(), put["fraction"].as_f64()), (Some(id.as_str()), Some("epubcfi(/6/4!/4/2/1:7)"), Some(0.42)));
    assert!(put["updated_at"].is_string());
    assert_eq!(call(&router, "GET", &uri, Some(TOKEN), None).await, (StatusCode::OK, put.clone()));
    let (_, clamped) = call(&router, "PUT", &uri, Some(TOKEN), Some(json!({ "position": "epubcfi(/6/6)", "fraction": 1.7 }))).await;
    assert_eq!(clamped["fraction"].as_f64(), Some(1.0));

    assert_eq!(call(&router, "PUT", "/articles/nope/progress", Some(TOKEN), Some(json!({ "position": "p", "fraction": 0.1 }))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(call(&router, "PUT", &uri, Some(TOKEN), Some(json!({ "position": "  ", "fraction": 0.1 }))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(call(&router, "PUT", &uri, Some(TOKEN), Some(json!({ "fraction": 0.1 }))).await.0, StatusCode::BAD_REQUEST);

    // The list shows the fraction.
    let (_, listed) = call(&router, "GET", "/articles", Some(TOKEN), None).await;
    assert_eq!(listed[0]["progress"].as_f64(), Some(1.0));

    // Highlights take an anchor and a chapter label; plain ones still work.
    let hl_uri = format!("/articles/{id}/highlights");
    let anchored = json!({ "text": "hold hands", "note": "cute", "anchor": "epubcfi(/6/14!/4/2,/1:0,/1:27)", "location": "Sleeping" });
    let (status, h) = call(&router, "POST", &hl_uri, Some(TOKEN), Some(anchored)).await;
    assert_eq!(status, StatusCode::CREATED, "{h}");
    assert_eq!((h["anchor"].as_str(), h["location"].as_str()), (Some("epubcfi(/6/14!/4/2,/1:0,/1:27)"), Some("Sleeping")));
    let (_, plain) = call(&router, "POST", &hl_uri, Some(TOKEN), Some(json!({ "text": "clams" }))).await;
    assert_eq!((plain["anchor"].clone(), plain["location"].clone()), (Value::Null, Value::Null));
    let (_, listed_hl) = call(&router, "GET", &hl_uri, Some(TOKEN), None).await;
    assert_eq!(listed_hl[0]["anchor"], "epubcfi(/6/14!/4/2,/1:0,/1:27)");

    // The export carries progress; deleting the book takes progress and highlights with it.
    let (_, export) = call(&router, "GET", "/export", Some(TOKEN), None).await;
    assert_eq!(export["progress"].as_array().map(Vec::len), Some(1));
    assert_eq!(export["articles"][0]["kind"], "book");
    assert_eq!(call(&router, "DELETE", &format!("/articles/{id}"), Some(TOKEN), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(call(&router, "GET", &uri, Some(TOKEN), None).await, (StatusCode::OK, Value::Null));
    assert_eq!(call(&router, "GET", &hl_uri, Some(TOKEN), None).await, (StatusCode::OK, json!([])));
    let (_, export) = call(&router, "GET", "/export", Some(TOKEN), None).await;
    assert_eq!(export["progress"], json!([]));
    // The same file again is a new book, since the old one is gone.
    assert_eq!(upload(&router, small_book()).await.0, StatusCode::CREATED);
}

#[tokio::test]
async fn embedded_web_files_are_public_and_everything_else_still_needs_the_token() {
    let (app, _dir) = app();
    let router = api::router(app, token_auth());

    let (status, headers, body) = raw(&router, "GET", "/vendor/README.txt", &[], vec![]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "text/plain; charset=utf-8");
    assert!(String::from_utf8_lossy(&body).contains("foliate-js"));
    let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().expect("csp");
    assert!(csp.contains("script-src 'self';") && csp.contains("frame-src 'self' blob:"), "{csp}");
    // Safari applies the inherited policy to the reader's blob: section frames, so 'none' would blank every book.
    assert!(csp.contains("frame-ancestors 'self'") && !csp.contains("frame-ancestors 'none'"), "{csp}");
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");

    // Not in the embedded table (or hidden, or a directory, or a traversal): not public, so 401 without the token.
    for path in ["/vendor/missing.js", "/vendor/", "/vendor", "/secret", "/vendor/../Cargo.toml", "/vendor/.hidden", "/%2e%2e/Cargo.toml"] {
        assert_eq!(raw(&router, "GET", path, &[], vec![]).await.0, StatusCode::UNAUTHORIZED, "{path}");
    }
    // With the token an unknown path is a plain 404, and a static path is GET only.
    let auth = auth_header();
    assert_eq!(raw(&router, "GET", "/vendor/missing.js", &[("authorization", &auth)], vec![]).await.0, StatusCode::NOT_FOUND);
    assert_eq!(raw(&router, "POST", "/vendor/README.txt", &[], vec![]).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(raw(&router, "POST", "/vendor/README.txt", &[("authorization", &auth)], vec![]).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn mcp_reads_books_a_page_at_a_time() {
    let (app, _dir) = app();
    let router = api::router(app.clone(), token_auth());
    // About 150k characters of "é" text, so slicing must count characters, not bytes.
    let (_, book) = upload(&router, epub("Long", &[("All", vec!["é".repeat(1000); 150])])).await;
    let id = book["id"].as_str().expect("id").to_string();
    let web_body = json!({ "url": "https://example.com/w", "title": "W", "markdown": "x".repeat(60_000) });
    let (_, web) = call(&router, "POST", "/articles", Some(TOKEN), Some(web_body)).await;
    let web_id = web["id"].as_str().expect("id").to_string();

    let (server_io, client_io) = tokio::io::duplex(1024 * 1024);
    let server = tokio::spawn(async move {
        let running = MarginaliaMcp::new(app).serve(server_io).await.expect("serve");
        let _ = running.waiting().await;
    });
    let client = ClientConfig::default().serve(client_io).await.expect("client");
    let get = |a: Value| {
        let client = &client;
        async move {
            let res = client.call_tool(CallToolRequestParams::new("get_article").with_arguments(args(a))).await.expect("get_article");
            assert_ne!(res.is_error, Some(true));
            serde_json::from_str::<Value>(&text_of(&res)).expect("json")
        }
    };

    let first = get(json!({ "id": id })).await;
    assert_eq!(first["kind"], "book");
    assert_eq!(first["offset"], 0);
    let total = first["total_chars"].as_u64().expect("total") as usize;
    assert!(total > 150_000, "{total}");
    assert_eq!(first["markdown"].as_str().expect("md").chars().count(), 40_000);
    assert!(first["markdown"].as_str().expect("md").starts_with("## All"));
    assert_eq!(first["next_offset"], 40_000);

    let mut seen = 40_000;
    let mut next = first["next_offset"].as_u64();
    while let Some(offset) = next {
        let page = get(json!({ "id": id, "offset": offset, "max_chars": 70_000 })).await;
        seen += page["markdown"].as_str().expect("md").chars().count();
        next = page["next_offset"].as_u64();
    }
    assert_eq!(seen, total);
    let past = get(json!({ "id": id, "offset": total + 5 })).await;
    assert_eq!((past["markdown"].as_str(), past["next_offset"].clone()), (Some(""), Value::Null));

    // A web article is whole unless asked otherwise, and has no paging fields.
    let whole = get(json!({ "id": web_id })).await;
    assert_eq!(whole["markdown"].as_str().map(str::len), Some(60_000));
    assert!(whole.get("next_offset").is_none());
    assert_eq!(whole["kind"], "web");
    let part = get(json!({ "id": web_id, "offset": 10, "max_chars": 5 })).await;
    assert_eq!((part["markdown"].as_str(), part["next_offset"].as_u64()), (Some("xxxxx"), Some(15)));

    // list_articles shows kind and author.
    let listed = client.call_tool(CallToolRequestParams::new("list_articles").with_arguments(args(json!({})))).await.expect("list");
    let rows: Value = serde_json::from_str(&text_of(&listed)).expect("json");
    assert!(rows.as_array().expect("rows").iter().any(|r| r["kind"] == "book" && r["author"] == "Test Author"));

    let _ = client.cancel().await;
    let _ = server.await;
}

#[tokio::test]
async fn cors_lets_the_app_put_progress_and_upload_books() {
    let (app, _dir) = app();
    let router = api::router(app, token_auth());
    let res = with_origin(
        &router,
        "OPTIONS",
        "/articles/x/progress",
        "tauri://localhost",
        &[("access-control-request-method", "PUT"), ("access-control-request-headers", "authorization,content-type,x-filename")],
    )
    .await;
    assert!(res.status().is_success(), "{}", res.status());
    assert!(hv(&res, "access-control-allow-methods").is_some_and(|m| m.contains("PUT")));
    assert!(hv(&res, "access-control-allow-headers").is_some_and(|h| h.to_ascii_lowercase().contains("x-filename")));
}
