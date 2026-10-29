//! Web page, sanitised HTML endpoints and the `/save` link. The router is driven in-process with `oneshot`;
//! MemRepo, FsBlobStore on a tempdir. The only socket is a local axum page server for the `/save` happy path.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use http_body_util::BodyExt;
use marginalia::api;
use marginalia::blob::FsBlobStore;
use marginalia::domain::{NewSynthesis, SynthesisKind};
use marginalia::mem::MemRepo;
use marginalia::service::{App, AppConfig};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tower::ServiceExt as _;

const TOKEN: &str = "s3cret";

fn app() -> (Arc<App>, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let app = App::new(Arc::new(MemRepo::new()), Arc::new(FsBlobStore::new(dir.path().join("blobs"))), AppConfig::default());
    (Arc::new(app), dir)
}

async fn get(router: &Router, uri: &str, token: Option<&str>) -> (Response<Body>, String) {
    let mut req = Request::builder().method("GET").uri(uri);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let res = router.clone().oneshot(req.body(Body::empty()).expect("request")).await.expect("response");
    let (parts, body) = res.into_parts();
    let bytes = body.collect().await.expect("body").to_bytes();
    (Response::from_parts(parts, Body::empty()), String::from_utf8_lossy(&bytes).into_owned())
}

fn header_of<'a>(res: &'a Response<Body>, name: header::HeaderName) -> &'a str {
    res.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

#[tokio::test]
async fn static_files_are_public_with_the_right_content_types() {
    let (app, _dir) = app();
    let router = api::router(app, Some(TOKEN.into()));

    for (path, content_type, needle) in [
        ("/", "text/html; charset=utf-8", "<title>Marginalia</title>"),
        ("/app.js", "text/javascript; charset=utf-8", "localStorage"),
        ("/app.css", "text/css; charset=utf-8", "prefers-color-scheme: dark"),
    ] {
        let (res, body) = get(&router, path, None).await;
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        assert_eq!(header_of(&res, header::CONTENT_TYPE), content_type, "{path}");
        assert_eq!(header_of(&res, header::X_CONTENT_TYPE_OPTIONS), "nosniff", "{path}");
        assert!(body.contains(needle), "{path} should contain {needle:?}");
    }
    let (res, _) = get(&router, "/", None).await;
    assert!(header_of(&res, header::CONTENT_SECURITY_POLICY).contains("script-src 'self'"));

    // The script origin stays locked down: no inline script, no eval. Styles may be inline (Mermaid's SVG needs it).
    let csp = header_of(&res, header::CONTENT_SECURITY_POLICY);
    let script_src = csp.split(';').map(str::trim).find(|d| d.starts_with("script-src")).expect("script-src directive");
    assert_eq!(script_src, "script-src 'self'", "{csp}");
    assert!(!csp.contains("unsafe-eval"), "{csp}");

    // The files hold no data, but the API still does.
    assert_eq!(get(&router, "/articles", None).await.0.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn article_html_needs_the_token_and_escapes_script() {
    let (app, _dir) = app();
    let article = app
        .save_markdown(
            "https://example.com/evil",
            "Evil",
            "# Heading\n\n<script>alert(1)</script>\n\n- item\n\n[click](javascript:alert(2)) and [ok](https://example.com/x)",
        )
        .await
        .expect("save");
    let router = api::router(app, Some(TOKEN.into()));
    let uri = format!("/articles/{}/html", article.id);

    assert_eq!(get(&router, &uri, None).await.0.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(get(&router, &uri, Some("wrong")).await.0.status(), StatusCode::UNAUTHORIZED);

    let (res, html) = get(&router, &uri, Some(TOKEN)).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(header_of(&res, header::CONTENT_TYPE).starts_with("text/html"));
    assert!(!html.contains("<script"), "{html}");
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"), "{html}");
    assert!(html.contains("<h1>Heading</h1>"), "{html}");
    assert!(!html.contains("javascript:"), "{html}");
    assert!(html.contains("href=\"https://example.com/x\" rel=\"noopener noreferrer\" target=\"_blank\""), "{html}");

    let (res, body) = get(&router, "/articles/nope/html", Some(TOKEN)).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    assert!(body.contains("error"));
}

#[tokio::test]
async fn synthesis_html_is_rendered_and_protected() {
    let (app, _dir) = app();
    let synthesis = app
        .repo()
        .save_synthesis(NewSynthesis {
            kind: SynthesisKind::Brief,
            title: "Brief".into(),
            direction: None,
            body: "## Findings\n\n1. one\n2. two <script>x</script>".into(),
            article_ids: vec![],
        })
        .await
        .expect("synthesis");
    let router = api::router(app, Some(TOKEN.into()));
    let uri = format!("/syntheses/{}/html", synthesis.id);

    assert_eq!(get(&router, &uri, None).await.0.status(), StatusCode::UNAUTHORIZED);
    let (res, html) = get(&router, &uri, Some(TOKEN)).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(html.contains("<h2>Findings</h2>") && html.contains("<ol>"), "{html}");
    assert!(!html.contains("<script"), "{html}");
    assert_eq!(get(&router, "/syntheses/nope/html", Some(TOKEN)).await.0.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn save_link_rejects_a_missing_or_bad_token() {
    let (app, _dir) = app();
    let router = api::router(app.clone(), Some(TOKEN.into()));
    // The URL is unreachable on purpose: a request that passes auth would try to fetch it, and these must not.
    let url = "url=http%3A%2F%2F127.0.0.1%3A9%2Fnever";

    for uri in [format!("/save?{url}"), format!("/save?{url}&token="), format!("/save?{url}&token=wrong"), format!("/save?{url}&token=s3cre")] {
        let (res, body) = get(&router, &uri, None).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert!(!body.contains(TOKEN));
    }
    // A bearer header does not stand in for the query token on this route, and the query token works nowhere else.
    assert_eq!(get(&router, &format!("/save?{url}"), Some(TOKEN)).await.0.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(get(&router, &format!("/articles?token={TOKEN}"), None).await.0.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(get(&router, &format!("/export?token={TOKEN}"), None).await.0.status(), StatusCode::UNAUTHORIZED);
    assert!(app.repo().list_articles(10, 0).await.expect("list").is_empty());

    // Right token, no url: a clear 400, still no redirect.
    let (res, body) = get(&router, &format!("/save?token={TOKEN}"), None).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(body.contains("Missing url"));
    assert!(!body.contains(TOKEN));
}

#[tokio::test]
async fn save_link_saves_then_redirects_to_the_reader() {
    let page = Router::new().route(
        "/sodium",
        axum::routing::get(|| async {
            let paragraphs: String = (0..8)
                .map(|i| format!("<p>Paragraph {i} explains how sodium-ion cells trade energy density for cost and safety, which matters for grid storage.</p>"))
                .collect();
            axum::response::Html(format!(
                "<html><head><title>Sodium cells | Site</title></head><body><article><h1>Sodium cells</h1>{paragraphs}</article></body></html>"
            ))
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, page).await;
    });

    let (app, _dir) = app();
    let router = api::router(app.clone(), Some(TOKEN.into()));
    let target = format!("http://{addr}/sodium");
    let (res, _) = get(&router, &format!("/save?token={TOKEN}&url={target}"), None).await;

    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let saved = app.repo().list_articles(10, 0).await.expect("list");
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].url, target);
    assert_eq!(header_of(&res, header::LOCATION), format!("/#/a/{}", saved[0].id));
    assert_eq!(header_of(&res, header::CACHE_CONTROL), "no-store");
    assert_eq!(header_of(&res, header::REFERRER_POLICY), "no-referrer");
}

#[tokio::test]
async fn save_link_reports_fetch_errors_as_a_page_without_the_token() {
    let (app, _dir) = app();
    let router = api::router(app, Some(TOKEN.into()));
    // Not http(s): rejected before any network use.
    let (res, body) = get(&router, &format!("/save?token={TOKEN}&url=ftp%3A%2F%2Fx.test%2F%3Cb%3E"), None).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(header_of(&res, header::CONTENT_TYPE).starts_with("text/html"));
    assert!(body.contains("must start with http"), "{body}");
    assert!(!body.contains(TOKEN));
}

#[tokio::test]
async fn save_link_without_a_configured_token_is_open_but_still_validates() {
    let (app, _dir) = app();
    let router = api::router(app, None);
    assert_eq!(get(&router, "/save", None).await.0.status(), StatusCode::BAD_REQUEST);
    assert_eq!(get(&router, "/save?url=ftp%3A%2F%2Fx.test", None).await.0.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn vendored_mermaid_is_public_cached_and_keeps_the_csp() {
    let (app, _dir) = app();
    let router = api::router(app, Some(TOKEN.into()));
    let req = Request::builder().method("GET").uri("/vendor/mermaid.min.js").body(Body::empty()).expect("request");
    let res = router.clone().oneshot(req).await.expect("response");
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(header_of(&res, header::CONTENT_TYPE), "text/javascript; charset=utf-8");
    assert_eq!(header_of(&res, header::X_CONTENT_TYPE_OPTIONS), "nosniff");
    assert!(header_of(&res, header::CACHE_CONTROL).contains("max-age=31536000"));
    let csp = header_of(&res, header::CONTENT_SECURITY_POLICY).to_owned();
    assert!(csp.contains("script-src 'self';"), "{csp}");
    assert!(!csp.contains("script-src 'self' 'unsafe"), "{csp}");
    let bytes = res.into_body().collect().await.expect("body").to_bytes();
    assert!(bytes.len() > 100_000, "mermaid should be a large script, got {} bytes", bytes.len());
    assert!(String::from_utf8_lossy(&bytes).contains("mermaid"));
}

#[tokio::test]
async fn mermaid_fences_render_with_the_class_the_page_script_looks_for() {
    let (app, _dir) = app();
    let article = app
        .save_markdown(
            "https://example.com/diagram",
            "Diagram",
            "Intro.\n\n```mermaid\nflowchart TD\n  A[Start] --> B{Is <it> ok?}\n```\n\n```rust\nfn main() {}\n```\n",
        )
        .await
        .expect("save");
    let router = api::router(app, Some(TOKEN.into()));
    let (res, html) = get(&router, &format!("/articles/{}/html", article.id), Some(TOKEN)).await;
    assert_eq!(res.status(), StatusCode::OK);
    // `pre > code.language-mermaid` is the selector in web/app.js.
    assert!(html.contains("<pre><code class=\"language-mermaid\">flowchart TD\n  A[Start] --&gt; B{Is &lt;it&gt; ok?}\n</code></pre>"), "{html}");
    assert!(html.contains("<pre><code class=\"language-rust\">"), "{html}");
    assert!(include_str!("../web/app.js").contains("pre > code.language-mermaid"));
}
