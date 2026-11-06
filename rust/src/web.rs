//! Static frontend hosting, the Rust half of `WebMvcConfig.addResourceHandlers`
//! + `ReactRoutingController`: serve the built SPA from disk and fall back to
//!   `index.html` for extensionless GET paths.
//!
//! Java resolves `customFiles/static/` first, then `classpath:/static/` (the
//! frontend build baked into the jar). Rust has no classpath, so the root is:
//!
//! 1. `RUST_STATIC_DIR` / `FRONTEND_DIST` env — explicit deploy override.
//! 2. `<STIRLING_BASE_PATH|.>/customFiles/static/` — the same directory Java
//!    reads, so a shared install works for both backends.
//!
//! Cache policy mirrors the Java resource handlers: no-store for service
//! worker/manifest, one-year immutable for `/assets/**` (Vite fingerprints),
//! one day + stale-while-revalidate for the stable asset dirs, no-cache for
//! the SPA fallback.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};

pub(crate) fn static_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("RUST_STATIC_DIR").or_else(|_| std::env::var("FRONTEND_DIST")) {
        let dir = dir.trim();
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    let base = match std::env::var("STIRLING_BASE_PATH") {
        Ok(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
        _ => PathBuf::from("."),
    };
    let dir = base.join("customFiles").join("static");
    dir.is_dir().then_some(dir)
}

/// Backend-owned first path segments that must never become SPA routes —
/// the Rust analogue of `ReactRoutingController.NON_SPA_FIRST_SEGMENTS`.
const NON_SPA: &[&str] = &[
    "api",
    "actuator",
    "internal",
    "v1",
    "swagger",
    "webjars",
    "js",
    "css",
    "fonts",
    "images",
    "assets",
    "locales",
    "pdfjs",
    "pdfium",
    "vendor",
    "samples",
    "icons",
    "modern-logo",
    "Login",
    "login",
    "health",
    "metrics",
    // Java's exclusion regexes name these too; without them a dot-free
    // `/pipeline` or `/static/x` request would wrongly get index.html.
    "static",
    "pipeline",
    "og_images",
];

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" | "webmanifest" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "txt" | "md" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "pdf" => "application/pdf",
        "toml" => "application/toml",
        "mp4" | "webm" => "video/mp4",
        "mp3" | "wav" | "ogg" => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

fn cache_control(rel: &str) -> &'static str {
    if matches!(
        rel,
        "sw.js" | "manifest.json" | "site.webmanifest" | "browserconfig.xml"
    ) {
        "no-store"
    } else if rel.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else if rel.starts_with("images/")
        || rel.starts_with("fonts/")
        || rel.starts_with("icons/")
        || rel.starts_with("modern-logo/")
        || rel.starts_with("locales/")
        || rel.starts_with("pdfjs/")
        || rel.starts_with("pdfium/")
        || rel.starts_with("vendor/")
        || rel.starts_with("samples/")
        || rel.starts_with("og_images/")
        || rel.starts_with("Login/")
        || rel.starts_with("css/")
        || rel.starts_with("js/")
        || rel.starts_with("3rdPartyLicenses.json")
        || rel.starts_with("favicon.")
        || rel.starts_with("apple-touch-icon")
        || rel.starts_with("android-chrome-")
        || rel.starts_with("mstile-")
        || rel.starts_with("safari-pinned-tab")
    {
        "public, max-age=86400, stale-while-revalidate=86400"
    } else {
        "no-cache"
    }
}

/// Resolve `path` under `root` without escaping it. Returns `None` for
/// traversal attempts, directories, and missing files.
fn resolve(root: &Path, path: &str) -> Option<PathBuf> {
    let rel = path.trim_start_matches('/');
    if rel.is_empty() || rel.split('/').any(|seg| seg == ".." || seg.contains('\\')) {
        return None;
    }
    let candidate = root.join(rel);
    if !candidate.is_file() {
        return None;
    }
    // Canonicalise both sides so a symlink inside the root cannot point out.
    let canon = candidate.canonicalize().ok()?;
    let root_canon = root.canonicalize().ok()?;
    canon.starts_with(&root_canon).then_some(canon)
}

fn serve_file(path: &Path, cache: &str) -> Response {
    match std::fs::read(path) {
        Ok(bytes) => {
            let mut res = Response::new(Body::from(bytes));
            let headers = res.headers_mut();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(content_type(path)),
            );
            headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_str(cache).unwrap_or(HeaderValue::from_static("no-cache")),
            );
            res
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `GET /robots.txt` — `RobotsController`: `system.googlevisibility`
/// picks allow-all vs disallow-all so embedded deployments stay out of
/// search engines.
pub async fn robots_txt() -> Response {
    let allow = crate::settings::current().system.google_visibility;
    let body = if allow {
        "User-agent: *\nAllow: /\n"
    } else {
        "User-agent: *\nDisallow: /\n"
    };
    let mut res = Response::new(Body::from(body));
    res.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    res
}

/// `ReactRoutingController`'s deploy base path: Java binds
/// `server.servlet.context-path` to `SYSTEM_ROOTURIPATH`. Rust does not
/// strip the prefix from request URIs (deploys that set it must front the
/// server with a rewrite), but the value is still baked into served HTML.
fn context_path() -> String {
    match std::env::var("SYSTEM_ROOTURIPATH") {
        Ok(p) if !p.trim().is_empty() => {
            let p = p.trim().to_string();
            if p.ends_with('/') {
                p
            } else {
                format!("{p}/")
            }
        }
        _ => "/".to_string(),
    }
}

/// `applyContextPath`: fill `%BASE_URL%`, rewrite the baked `<base href>`,
/// and expose the API base to the SPA.
/// Java escapes the context path for HTML-attribute and JS-string
/// contexts separately (`HtmlUtils`/`JavaScriptUtils`); the env value is
/// admin-controlled but still goes through both.
fn escape_html_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn escape_js_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "")
}

fn process_html(html: &str, base_url: &str) -> String {
    let mut out = html.replace("%BASE_URL%", base_url);
    if let Some(start) = out.find("<base href=\"") {
        let rest = &out[start + "<base href=\"".len()..];
        if let Some(end) = rest.find('"') {
            let tag_end = rest[end..].find('>').map(|i| end + i + 1);
            if let Some(tag_end) = tag_end {
                out.replace_range(
                    start..start + "<base href=\"".len() + tag_end,
                    &format!("<base href=\"{}\" />", escape_html_attr(base_url)),
                );
            }
        }
    }
    let script = format!(
        "<script>window.STIRLING_PDF_API_BASE_URL = '{}';</script>",
        escape_js_string(base_url)
    );
    if let Some(pos) = out.find("</head>") {
        out.insert_str(pos, &script);
    }
    out
}

fn read_static(root: &Path, name: &str) -> Option<String> {
    let file = resolve(root, name)?;
    std::fs::read_to_string(file).ok()
}

/// Java's `SAFE_SEGMENT` for prerendered route keys.
fn safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Static HTML names that are pages, not prerendered SPA routes —
/// `ReactRoutingController.NON_ROUTE_HTML`.
const NON_ROUTE_HTML: &[&str] = &[
    "index",
    "api-landing",
    "saas-landing",
    "mobile-upload",
    "mobile-sign",
];

/// `<route>.html` prerendered page for a clean URL, or `None`. Only 1–2
/// segment keys matching Java's `addPrerenderedRoute` rules qualify.
fn prerendered(root: &Path, path: &str, base_url: &str) -> Option<String> {
    let rel = path.trim_start_matches('/');
    let segments: Vec<&str> = rel.split('/').collect();
    if segments.is_empty() || segments.len() > 2 || !segments.iter().all(|s| safe_segment(s)) {
        return None;
    }
    let key = segments.join("/");
    if NON_ROUTE_HTML.contains(&key.as_str()) {
        return None;
    }
    let html = read_static(root, &format!("{key}.html"))?;
    Some(process_html(&html, base_url))
}

fn serve_html(body: String) -> Response {
    let mut res = Response::new(Body::from(body));
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    // Java's page responses use CacheControl.noCache().mustRevalidate().
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, must-revalidate"),
    );
    res
}

/// `serveIndexHtml`: the normal SPA shell — `index.html` with the context
/// path applied, or the generated fallback when no shell was deployed.
fn index_html(root: &Path, base_url: &str) -> Response {
    match read_static(root, "index.html") {
        Some(html) => serve_html(process_html(&html, base_url)),
        None => serve_html(fallback_html(base_url)),
    }
}

/// `serveRootPage`: ONLY `/` and `/index.html` swap in `saas-landing.html`
/// when it exists (the SaaS classpath artefact); every other caller of
/// `serveIndexHtml` keeps the normal shell.
fn root_page(root: &Path, base_url: &str) -> Response {
    if let Some(html) = read_static(root, "saas-landing.html") {
        return serve_html(process_html(&html, base_url));
    }
    index_html(root, base_url)
}

/// Java's `buildFallbackHtml`: minimal page that still completes the
/// desktop SSO deep-link handoff when no index.html was deployed.
fn fallback_html(base_url: &str) -> String {
    let base_html = escape_html_attr(base_url);
    let base_js = escape_js_string(base_url);
    let server_url = format!("(window.location.origin + '{base_js}')");
    format!(
        r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <base href="{base_html}" />
    <title>Stirling PDF</title>
    <script>
      (function() {{
        const baseUrl = '{base_js}';
        window.STIRLING_PDF_API_BASE_URL = baseUrl;
        const hashParams = new URLSearchParams(window.location.hash.replace(/^#/, ''));
        const searchParams = new URLSearchParams(window.location.search);
        const token = hashParams.get('access_token') || hashParams.get('token') || searchParams.get('access_token');
        const serverUrl = {server_url};

        if (token) {{
          const nonceFromUrl = hashParams.get('nonce') || searchParams.get('nonce');
          console.log('[Fallback Auth] Token received, sending to desktop app via deep link');
          try {{
            const encodedToken = encodeURIComponent(token);
            const encodedServer = encodeURIComponent(serverUrl);
            const encodedNonce = nonceFromUrl ? encodeURIComponent(nonceFromUrl) : '';
            const deepLink = `stirlingpdf://auth/sso-complete?server=${{encodedServer}}#access_token=${{encodedToken}}&nonce=${{encodedNonce}}&type=sso-selfhosted`;
            window.location.href = deepLink;
            return;
          }} catch (_) {{
          }}
        }}
      }})();
    </script>
  </head>
  <body>
    <p>Stirling PDF is running.</p>
  </body>
</html>
"#
    )
}

/// Java's `buildCallbackHtml`: the `/auth/callback/tauri` page that forwards
/// the OAuth result to the desktop app over the `stirlingpdf://` deep link.
fn callback_html(base_url: &str) -> String {
    let base_html = escape_html_attr(base_url);
    let base_js = escape_js_string(base_url);
    let server_url = format!("(window.location.origin + '{base_js}')");
    format!(
        r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8" />
    <base href="{base_html}" />
    <title>Authentication Complete</title>
    <style>
      * {{ margin: 0; padding: 0; box-sizing: border-box; }}
      body {{
        font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, 'Helvetica Neue', Arial, sans-serif;
        text-align: center; padding: 50px 20px; background: #f5f5f5;
        min-height: 100vh; display: flex; align-items: center; justify-content: center;
      }}
      .container {{
        background: #ffffff; border-radius: 12px; padding: 40px; max-width: 420px;
        width: 100%; box-shadow: 0 2px 8px rgba(0,0,0,0.1); border: 1px solid #e5e7eb; color: #1a1a1a;
      }}
      .icon {{ font-size: 48px; margin-bottom: 16px; color: #2e7d32; }}
      .icon.error {{ color: #d32f2f; }}
      h1 {{ font-size: 24px; font-weight: 600; margin-bottom: 12px; color: #1a1a1a; }}
      p {{ color: #666; line-height: 1.6; font-size: 15px; }}
      .error-details {{
        background: #ffebee; border: 1px solid #ffcdd2; padding: 16px; border-radius: 8px;
        margin-top: 20px; font-size: 14px; color: #c62828; word-break: break-word;
        text-align: left; line-height: 1.5; display: none;
      }}
      @media (prefers-color-scheme: dark) {{
        body {{ background: #1a1a1a; color: #e0e0e0; }}
        .container {{ background: #2d2d2d; box-shadow: 0 2px 8px rgba(0,0,0,0.3); border-color: #374151; color: #e5e7eb; }}
        .icon {{ color: #66bb6a; }}
        .icon.error {{ color: #ef5350; }}
        h1 {{ color: #f5f5f5; }}
        p {{ color: #b0b0b0; }}
        .error-details {{ background: #3d2020; border: 1px solid #5d3030; color: #ef9a9a; }}
      }}
      @media (max-width: 480px) {{
        body {{ padding: 20px 16px; }}
        .container {{ padding: 32px 24px; }}
        h1 {{ font-size: 20px; }}
        .icon {{ font-size: 40px; }}
      }}
    </style>
    <script>
      (function() {{
        const run = () => {{
          const hashParams = new URLSearchParams(window.location.hash.replace(/^#/, ''));
          const searchParams = new URLSearchParams(window.location.search);
          const token = hashParams.get('access_token') || hashParams.get('token') || searchParams.get('access_token');
          const errorCode = searchParams.get('errorOAuth')
            || searchParams.get('error')
            || hashParams.get('error')
            || searchParams.get('error_description')
            || hashParams.get('error_description');
          const serverUrl = {server_url};
          const iconEl = document.getElementById('auth-icon');
          const titleEl = document.getElementById('auth-title');
          const messageEl = document.getElementById('auth-message');
          const detailsEl = document.getElementById('auth-error-details');

          const sendDeepLink = (type, value, key) => {{
            try {{
              const encodedValue = encodeURIComponent(value || '');
              const encodedServer = encodeURIComponent(serverUrl);
              const hashKey = key || 'access_token';
              const deepLink = `stirlingpdf://auth/sso-complete?server=${{encodedServer}}#${{hashKey}}=${{encodedValue}}&type=${{type}}`;
              window.location.href = deepLink;
            }} catch (_) {{
            }}
          }};

          const showError = (message, details) => {{
            if (iconEl) {{ iconEl.textContent = '✗'; iconEl.classList.add('error'); }}
            if (titleEl) {{ titleEl.textContent = 'Authentication failed'; }}
            if (messageEl) {{ messageEl.textContent = message; }}
            if (detailsEl && details) {{
              detailsEl.textContent = details;
              detailsEl.style.display = 'block';
            }}
          }};

          if (token) {{
            const nonceFromUrl = hashParams.get('nonce') || searchParams.get('nonce');
            console.log('[Auth Callback] Token received, sending to desktop app via deep link');
            setTimeout(() => {{
              try {{
                const encodedToken = encodeURIComponent(token);
                const encodedServer = encodeURIComponent(serverUrl);
                const encodedNonce = nonceFromUrl ? encodeURIComponent(nonceFromUrl) : '';
                const deepLink = `stirlingpdf://auth/sso-complete?server=${{encodedServer}}#access_token=${{encodedToken}}&nonce=${{encodedNonce}}&type=sso-selfhosted`;
                window.location.href = deepLink;
              }} catch (err) {{
                console.error('[Auth Callback] Failed to trigger deep link:', err);
              }}
            }}, 200);
            return;
          }}

          if (errorCode) {{
            const isCancelled = errorCode === 'access_denied';
            sendDeepLink('sso-error', errorCode, 'error');
            showError(
              isCancelled
                ? 'Authentication was cancelled. You can close this window and return to the app.'
                : 'Authentication was not successful. You can close this window and return to the app.',
              errorCode
            );
            return;
          }}

          showError(
            'Authentication did not complete. You can close this window and try again.',
            'missing_token'
          );
        }};

        if (document.readyState === 'loading') {{
          document.addEventListener('DOMContentLoaded', run);
        }} else {{
          run();
        }}
      }})();
    </script>
  </head>
  <body>
    <div class="container">
      <div class="icon" id="auth-icon">&#10003;</div>
      <h1 id="auth-title">Authentication complete</h1>
      <p id="auth-message">You can close this window and return to Stirling PDF.</p>
      <div class="error-details" id="auth-error-details"></div>
    </div>
  </body>
</html>
"#
    )
}

pub(crate) fn tauri_mode() -> bool {
    std::env::var("STIRLING_PDF_TAURI_MODE").is_ok_and(|v| v == "true")
}

/// `CleanUrlInterceptor.ALLOWED_PARAMS`: non-API GETs keep only these
/// query keys; anything else triggers a redirect to the cleaned URL.
const ALLOWED_QUERY_PARAMS: &[&str] = &[
    "lang",
    "endpoint",
    "endpoints",
    "logout",
    "error",
    "days",
    "date",
    "errorOAuth",
    "file",
    "messageType",
    "infoMessage",
    "page",
    "size",
    "type",
    "principal",
    "startDate",
    "endDate",
    "async",
    "session",
    "all",
];

/// `CleanUrlInterceptor.preHandle`: when a non-`/api/` request carries
/// query params outside the allowlist, redirect to the URL with only the
/// allowed ones. Returns the redirect target, or `None` to continue.
fn clean_url_redirect(path: &str, query: &str) -> Option<String> {
    if path.contains("/api/") || query.is_empty() {
        return None;
    }
    let account_link_callback = path == "/account-link/callback";
    // Java collects allowed pairs into a LinkedHashMap (dedup by key,
    // last value wins, insertion order kept) and redirects whenever its
    // size differs from the raw parameter count — covering both dropped
    // keys and duplicate allowed keys.
    let mut kept: Vec<(&str, Option<&str>)> = Vec::new();
    // Java's `String.split` drops trailing empty strings; `&`-splitting in
    // Rust keeps them, so trim them to match the parameter count.
    let pairs: Vec<&str> = {
        let mut v: Vec<&str> = query.split('&').collect();
        while v.last().is_some_and(|s| s.is_empty()) {
            v.pop();
        }
        v
    };
    let total = pairs.len();
    for pair in pairs {
        let mut kv = pair.splitn(2, '=');
        let key = kv.next().unwrap_or("");
        let value = kv.next();
        // Java's `split("=", -1)` yields >2 parts for `a=b=c` — dropped.
        if key.is_empty() || pair.matches('=').count() > 1 {
            continue;
        }
        if ALLOWED_QUERY_PARAMS.contains(&key) || (account_link_callback && key == "state") {
            if let Some(existing) = kept.iter_mut().find(|(ek, _)| *ek == key) {
                existing.1 = value;
            } else {
                kept.push((key, value));
            }
        }
    }
    if kept.len() == total {
        return None;
    }
    let cleaned = kept
        .iter()
        .map(|(k, v)| match v {
            Some(v) => format!("{k}={v}"),
            None => (*k).to_string(),
        })
        .collect::<Vec<_>>()
        .join("&");
    Some(format!("{path}?{cleaned}"))
}

/// Fallback entry point for non-`/api/` requests: static file → special
/// pages → prerendered route → SPA `index.html` → generated fallback →
/// 404. Mirrors `ReactRoutingController`.
pub async fn serve(req: Request) -> Response {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    let Some(root) = static_root() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path = req.uri().path().to_string();
    let first = path.split('/').nth(1).unwrap_or("");
    let base_url = context_path();

    // `/` and `/index.html` are controller routes in Java, not static
    // files: the @GetMapping wins over the resource handler, so the shell
    // gets context-path processing and the SaaS landing swap. The
    // CleanUrlInterceptor still runs first, as on every controller route.
    if path == "/" || path == "/index.html" {
        if let Some(target) = clean_url_redirect(&path, req.uri().query().unwrap_or("")) {
            let mut res = StatusCode::FOUND.into_response();
            match HeaderValue::from_str(&target) {
                Ok(v) => {
                    res.headers_mut().insert(header::LOCATION, v);
                }
                Err(_) => return StatusCode::BAD_REQUEST.into_response(),
            }
            return res;
        }
        return root_page(&root, &base_url);
    }

    if let Some(file) = resolve(&root, &path) {
        let rel = path.trim_start_matches('/').to_string();
        return serve_file(&file, cache_control(&rel));
    }

    // Bundled signature fonts: Java's `classpath:static/fonts/*.woff2`
    // resource handler. Custom `customFiles/static/fonts/` files already
    // won above via `resolve`.
    if let Some(name) = path.strip_prefix("/fonts/") {
        if !name.contains('/') && !name.contains("..") {
            if let Some(bytes) = crate::fonts::bytes(name) {
                let mut res = Response::new(Body::from(bytes));
                let headers = res.headers_mut();
                headers.insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static(content_type(Path::new(name))),
                );
                headers.insert(
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("public, max-age=86400, stale-while-revalidate=86400"),
                );
                return res;
            }
        }
    }

    // CleanUrlInterceptor runs on controller-mapped routes — the SPA
    // pages below — but not on static resource handlers. `/index.html`
    // is the only dotted page route.
    let cleanable =
        !NON_SPA.contains(&first) && (path == "/" || path == "/index.html" || !path.contains('.'));
    if cleanable {
        if let Some(target) = clean_url_redirect(&path, req.uri().query().unwrap_or("")) {
            let mut res = StatusCode::FOUND.into_response();
            match HeaderValue::from_str(&target) {
                Ok(v) => {
                    res.headers_mut().insert(header::LOCATION, v);
                }
                // Java answers 400 when the rebuilt URI is unparseable.
                Err(_) => return StatusCode::BAD_REQUEST.into_response(),
            }
            return res;
        }
    }

    // ReactRoutingController's named routes. `/auth/callback` and
    // `/share/{token}` serve the plain index shell — never a prerendered
    // page — matching Java's explicit @GetMappings ahead of the forwards.
    match path.as_str() {
        "/auth/callback" | "/auth/callback/tauri" => {
            return if path == "/auth/callback/tauri" {
                serve_html(callback_html(&base_url))
            } else {
                index_html(&root, &base_url)
            };
        }
        "/mobile-scanner" => {
            if tauri_mode() {
                if let Some(html) = read_static(&root, "mobile-upload.html") {
                    return serve_html(process_html(&html, &base_url));
                }
            }
            return index_html(&root, &base_url);
        }
        "/mobile-sign" => {
            if tauri_mode() {
                if let Some(html) = read_static(&root, "mobile-sign.html") {
                    return serve_html(process_html(&html, &base_url));
                }
            }
            return index_html(&root, &base_url);
        }
        _ => {}
    }
    // `/share/{token}` is a single-segment route; deeper /share paths fall
    // through to the SPA fallback like Java's forwardNestedPaths.
    if let Some(token) = path.strip_prefix("/share/") {
        if !token.is_empty() && !token.contains('/') {
            return index_html(&root, &base_url);
        }
    }

    // SPA fallback: extensionless GETs under non-backend prefixes get the
    // prerendered route page when one exists, else index.html — matching
    // `isSpaFallbackRoute` + `servePrerenderedOrIndex` (Java does not
    // check Accept, so neither do we).
    let is_spa_candidate = !path.contains('.') && !NON_SPA.contains(&first) && path != "/";
    if is_spa_candidate {
        if let Some(html) = prerendered(&root, &path, &base_url) {
            return serve_html(html);
        }
        return index_html(&root, &base_url);
    }
    StatusCode::NOT_FOUND.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types_cover_the_spa_surface() {
        assert_eq!(
            content_type(Path::new("index.html")),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("app.js")),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type(Path::new("site.webmanifest")),
            "application/json"
        );
        assert_eq!(content_type(Path::new("logo.svg")), "image/svg+xml");
        assert_eq!(content_type(Path::new("font.woff2")), "font/woff2");
        assert_eq!(content_type(Path::new("doc.pdf")), "application/pdf");
        assert_eq!(
            content_type(Path::new("data.bin")),
            "application/octet-stream"
        );
        assert_eq!(content_type(Path::new("noext")), "application/octet-stream");
    }

    #[test]
    fn cache_policy_matches_java_resource_handlers() {
        assert_eq!(cache_control("sw.js"), "no-store");
        assert_eq!(cache_control("manifest.json"), "no-store");
        assert_eq!(
            cache_control("assets/index-abc.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            cache_control("images/logo.png"),
            "public, max-age=86400, stale-while-revalidate=86400"
        );
        assert_eq!(
            cache_control("locales/en.json"),
            "public, max-age=86400, stale-while-revalidate=86400"
        );
        assert_eq!(cache_control("index.html"), "no-cache");
        assert_eq!(
            cache_control("favicon.ico"),
            "public, max-age=86400, stale-while-revalidate=86400"
        );
    }

    #[test]
    fn resolve_serves_files_but_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("index.html"), "<html></html>").unwrap();
        std::fs::create_dir(root.join("assets")).unwrap();
        std::fs::write(root.join("assets/app.js"), "x").unwrap();

        assert!(resolve(root, "/index.html").is_some());
        assert!(resolve(root, "assets/app.js").is_some());
        // Traversal and backslash tricks never escape the root.
        assert!(resolve(root, "/../etc/passwd").is_none());
        assert!(resolve(root, "/assets/../../secret").is_none());
        assert!(resolve(root, "/..\\win.ini").is_none());
        // Directories and missing files are not served.
        assert!(resolve(root, "/assets").is_none());
        assert!(resolve(root, "/missing.txt").is_none());
        assert!(resolve(root, "/").is_none());
    }

    #[test]
    fn non_spa_segments_cover_backend_prefixes() {
        for seg in [
            "api", "actuator", "internal", "v1", "swagger", "js", "health",
        ] {
            assert!(
                NON_SPA.contains(&seg),
                "{seg} must stay off the SPA fallback"
            );
        }
    }

    #[test]
    fn prerendered_serves_route_html_and_rejects_unsafe_keys() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("compress.html"), "<html>compress</html>").unwrap();
        std::fs::create_dir(root.join("settings")).unwrap();
        std::fs::write(root.join("settings/people.html"), "<html>people</html>").unwrap();
        std::fs::write(root.join("index.html"), "<html>index</html>").unwrap();

        let page = prerendered(root, "/compress", "/").unwrap();
        assert!(page.contains("compress"));
        let nested = prerendered(root, "/settings/people", "/").unwrap();
        assert!(nested.contains("people"));
        // index.html is a page, not a prerendered route.
        assert!(prerendered(root, "/index", "/").is_none());
        // Three segments, dots, and unsafe characters never resolve.
        assert!(prerendered(root, "/a/b/c", "/").is_none());
        assert!(prerendered(root, "/compress.html", "/").is_none());
        assert!(prerendered(root, "/..%2fetc", "/").is_none());
        assert!(prerendered(root, "/missing", "/").is_none());
    }

    #[test]
    fn process_html_applies_context_path() {
        let html = r#"<html><head><base href="/old/" /><title>t</title></head><body>%BASE_URL%</body></html>"#;
        let out = process_html(html, "/pdf/");
        assert!(out.contains("<base href=\"/pdf/\" />"));
        assert!(out.contains("window.STIRLING_PDF_API_BASE_URL = '/pdf/';"));
        assert!(out.contains("/pdf/</body>"));
        assert!(!out.contains("%BASE_URL%"));
        // No <base> tag: only the script injection and placeholder apply.
        let bare = process_html("<html><head></head><body>x</body></html>", "/");
        assert!(bare.contains("STIRLING_PDF_API_BASE_URL = '/';"));
    }

    #[tokio::test]
    async fn saas_landing_only_swaps_the_root_page() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("index.html"), "<html>app</html>").unwrap();
        std::fs::write(root.join("saas-landing.html"), "<html>saas</html>").unwrap();

        // Java's serveRootPage swaps; serveIndexHtml callers do not.
        let body = root_page(root, "/");
        let text = String::from_utf8(
            axum::body::to_bytes(body.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(text.contains("saas"));
    }

    #[test]
    fn fallback_and_callback_pages_embed_deep_link_handler() {
        let fallback = fallback_html("/");
        assert!(fallback.contains("stirlingpdf://auth/sso-complete"));
        assert!(fallback.contains("window.STIRLING_PDF_API_BASE_URL"));
        let callback = callback_html("/");
        assert!(callback.contains("stirlingpdf://auth/sso-complete"));
        assert!(callback.contains("Authentication complete"));
        assert!(fallback.contains("<base href=\"/\" />"));
        assert!(callback_html("/pdf/").contains("<base href=\"/pdf/\" />"));
    }

    #[test]
    fn clean_url_redirect_keeps_only_allowed_params() {
        // All allowed → no redirect.
        assert_eq!(clean_url_redirect("/tools", "lang=en&page=2"), None);
        // Disallowed keys drop; bare flags survive.
        assert_eq!(
            clean_url_redirect("/share/tok", "token=abc&lang=en&all"),
            Some("/share/tok?lang=en&all".to_string())
        );
        // Duplicates collapse last-wins, matching the LinkedHashMap.
        assert_eq!(
            clean_url_redirect("/", "lang=en&lang=fr"),
            Some("/?lang=fr".to_string())
        );
        // `a=b=c` is malformed in Java and dropped.
        assert_eq!(
            clean_url_redirect("/x", "lang=en&a=b=c"),
            Some("/x?lang=en".to_string())
        );
        // Everything dropped still redirects to the bare `?` form.
        assert_eq!(clean_url_redirect("/x", "foo=1"), Some("/x?".to_string()));
        // /api/ paths are exempt; trailing & doesn't count as a param.
        assert_eq!(clean_url_redirect("/api/v1/x", "foo=1"), None);
        assert_eq!(clean_url_redirect("/x", "lang=en&"), None);
        // `state` survives only on the account-link callback.
        assert_eq!(
            clean_url_redirect("/account-link/callback", "code=c&state=s"),
            Some("/account-link/callback?state=s".to_string())
        );
        assert_eq!(
            clean_url_redirect("/other", "state=s&lang=en"),
            Some("/other?lang=en".to_string())
        );
    }
}
