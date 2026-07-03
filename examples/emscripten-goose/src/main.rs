//! A Cloudflare Worker, written in Rust and compiled to
//! `wasm32-unknown-emscripten`, that uses the [`goose`] LLM library to turn a
//! prompt into a web page. The request's query string *is* the prompt; the
//! worker asks a model through Cloudflare's [AI Gateway] OpenAI-compatible REST
//! API to emit an HTML document and serves the result directly.
//!
//! The point of the example is that this works at all on emscripten: the call
//! goes goose -> reqwest -> hyper -> tokio `TcpStream`, with hostname resolution
//! via tokio's async DNS and TLS via rustls + ring, none of which is available
//! on `wasm32-unknown-unknown`.
//!
//! Configure via worker vars/secrets (see `wrangler.toml`):
//!   * `CLOUDFLARE_ACCOUNT_ID` — your account id.
//!   * `CF_AIG_GATEWAY_ID` — the gateway name. The base URL is derived from
//!     both as `https://gateway.ai.cloudflare.com/v1/<account>/<gateway>/compat`.
//!   * `OPENAI_MODEL` — a `provider/model` id (e.g. `openai/gpt-4.1`).
//!   * `CLOUDFLARE_API_TOKEN` (or `OPENAI_API_KEY`) — optional bearer token;
//!     omit it for an unauthenticated gateway.
//!   * `OPENAI_BASE_URL` — optional full override (host up to, not including,
//!     the `chat/completions` path — i.e. ending in `/compat`).
//!
//! [AI Gateway]: https://developers.cloudflare.com/ai-gateway/usage/rest-api/
use wasm_bindgen::prelude::*;
use web_sys::{Request, Response, ResponseInit};

// `main` runs automatically on init (the emscripten idiom); the worker entry
// point is the `fetch` export below.
fn main() {}

// The gateway compat base is the host up to (not including) `chat/completions`:
// we point `OpenAiProvider` at it with a `chat/completions` base path, yielding
// `.../compat/chat/completions`.
const DEFAULT_MODEL: &str = "openai/gpt-4.1";
const DEFAULT_PROMPT: &str = "a friendly hello-world landing page";
// goose defaults to `v1/chat/completions`; the gateway compat endpoint already
// has `/compat` in the base, so we drop the `v1` segment.
const COMPAT_BASE_PATH: &str = "chat/completions";

/// Build the AI Gateway OpenAI-compat base URL from an account id and gateway.
fn gateway_compat_base_url(account_id: &str, gateway_id: &str) -> String {
    format!("https://gateway.ai.cloudflare.com/v1/{account_id}/{gateway_id}/compat")
}

const SYSTEM_PROMPT: &str = "You are a web page generator. Reply with a single, complete, \
self-contained HTML document and nothing else: no commentary, no markdown code fences. \
All CSS must be inline in a <style> tag (no external stylesheets or <link>s), and all images \
must be inline as data: URIs or inline SVG — never reference external image URLs.";
const PROMPT_PREFIX: &str = "Output an HTML file for the following request: ";

// The landing page: a form that GETs `/generate?prompt=...`, which the worker
// then turns into a generated page.
const LANDING_PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>goose web generator</title>
<style>
  :root { color-scheme: dark; }
  body { margin:0; min-height:100vh; display:grid; place-items:center;
         font-family: system-ui, sans-serif; background:#0f172a; color:#e2e8f0; }
  .card { width:min(90vw,34rem); padding:2rem; }
  h1 { font-size:1.6rem; margin:0 0 1.25rem; }
  form { display:flex; gap:.5rem; }
  input { flex:1; padding:.8rem 1rem; border-radius:.6rem; border:1px solid #334155;
          background:#1e293b; color:inherit; font-size:1rem; }
  button { padding:.8rem 1.4rem; border:0; border-radius:.6rem; cursor:pointer;
           background:#38bdf8; color:#0f172a; font-weight:600; font-size:1rem;
           display:inline-flex; align-items:center; gap:.5rem; }
  button:disabled { cursor:progress; opacity:.85; }
  .spinner { width:1rem; height:1rem; border:2px solid #0f172a40; border-top-color:#0f172a;
             border-radius:50%; animation:spin .7s linear infinite; }
  @keyframes spin { to { transform:rotate(360deg); } }
  p { color:#94a3b8; font-size:.875rem; margin-top:1rem; }
</style>
</head>
<body>
  <div class="card">
    <h1>Generate a web page</h1>
    <form action="/generate" method="get">
      <input name="prompt" placeholder="type a website to generate…" autofocus required>
      <button type="submit">Generate</button>
    </form>
    <p>e.g. &ldquo;a neon synthwave landing page for a coffee shop&rdquo;</p>
  </div>
  <script>
    const form = document.querySelector("form");
    const btn = document.querySelector("button");
    form.addEventListener("submit", () => {
      document.querySelector("input").readOnly = true;
      btn.disabled = true;
      btn.innerHTML = '<span class="spinner"></span>Generating…';
    });
  </script>
</body>
</html>
"#;

#[wasm_bindgen(tokio, js_namespace = ["default"])]
pub async fn fetch(request: Request, env: JsValue, _ctx: JsValue) -> Result<Response, JsValue> {
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&format!("RUST PANIC: {info}").into());
    }));

    // Serve the landing form for everything except `/generate`.
    if path_of(&request).as_deref() != Some("/generate") {
        return respond(200, "text/html; charset=utf-8", LANDING_PAGE);
    }

    let prompt = query_param(&request, "prompt").unwrap_or_else(|| DEFAULT_PROMPT.to_string());
    // `OPENAI_BASE_URL` is a full override (ending in `/compat`); otherwise
    // derive it from the account id and gateway name.
    let base_url = env_string(&env, "OPENAI_BASE_URL").or_else(|| {
        let account_id = env_string(&env, "CLOUDFLARE_ACCOUNT_ID")?;
        let gateway_id = env_string(&env, "CF_AIG_GATEWAY_ID")?;
        Some(gateway_compat_base_url(&account_id, &gateway_id))
    });
    let model = env_string(&env, "OPENAI_MODEL").unwrap_or_else(|| DEFAULT_MODEL.into());
    let api_key =
        env_string(&env, "CLOUDFLARE_API_TOKEN").or_else(|| env_string(&env, "OPENAI_API_KEY"));

    let (status, content_type, body) = match base_url {
        Some(base_url) => match generate_page(&base_url, &model, api_key.as_deref(), &prompt).await {
            Ok(html) => (200, "text/html; charset=utf-8", html),
            Err(e) => (502, "text/plain; charset=utf-8", format!("error: {e}")),
        },
        None => (
            500,
            "text/plain; charset=utf-8",
            "error: set CLOUDFLARE_ACCOUNT_ID + CF_AIG_GATEWAY_ID (or OPENAI_BASE_URL)".to_string(),
        ),
    };

    respond(status, content_type, &body)
}

/// Build a `Response` with the given status, content type, and body.
fn respond(status: u16, content_type: &str, body: &str) -> Result<Response, JsValue> {
    let init = ResponseInit::new();
    init.set_status(status);
    let headers = web_sys::Headers::new()?;
    headers.set("content-type", content_type)?;
    init.set_headers(&headers);
    Response::new_with_opt_str_and_init(Some(body), &init)
}

/// Ask the model to render `prompt` as an HTML document and return it.
async fn generate_page(
    base_url: &str,
    model: &str,
    api_key: Option<&str>,
    prompt: &str,
) -> Result<String, String> {
    use goose::conversation::message::Message;
    use goose::providers::api_client::{ApiClient, AuthMethod};
    use goose::providers::base::Provider;
    use goose::providers::openai::OpenAiProviderBuilder;
    use goose_providers::model::ModelConfig;

    ensure_ring_provider();

    // reqwest's `GaiResolver` runs `getaddrinfo` on the blocking pool, which on
    // emscripten only answers from the resolution cache; tokio's async DNS
    // populates that cache without blocking the host event loop.
    if let Some(host) = host_of(base_url) {
        dns_prewarm(&host).await?;
    }

    // Send a bearer token when one is configured; an unauthenticated gateway
    // needs no auth.
    let auth = match api_key {
        Some(key) => AuthMethod::BearerToken(key.to_string()),
        None => AuthMethod::NoAuth,
    };
    let client = ApiClient::new_with_tls(base_url.to_string(), auth, None)
        .map_err(|e| format!("ApiClient::new_with_tls: {e}"))?;
    // Without an explicit cap goose sends a 4096-token default, which truncates
    // richer pages mid-document; give the model room to finish.
    let model_cfg = ModelConfig::new(model).with_max_tokens(Some(16_384));
    // The compat endpoint's path already includes `/compat`, so override
    // goose's default `v1/chat/completions` base path. Streaming is disabled:
    // Workers AI's compat stream can emit a non-string `delta.content` (e.g. an
    // integer) that goose's streaming parser rejects; the non-streaming path
    // returns a single clean JSON response.
    let provider = OpenAiProviderBuilder::new(client)
        .base_path(COMPAT_BASE_PATH)
        .supports_streaming(false)
        .build();

    let messages = [Message::user().with_text(format!("{PROMPT_PREFIX}{prompt}"))];
    let (reply, _usage) = provider
        .complete(&model_cfg, SYSTEM_PROMPT, &messages, &[])
        .await
        .map_err(|e| format!("provider.complete: {}", err_chain(&e)))?;

    let text = reply.as_concat_text();
    let html = strip_code_fence(text.trim());
    if html.is_empty() {
        return Err("model returned an empty response".into());
    }
    Ok(html.to_string())
}

/// Models sometimes wrap output in a ```html ... ``` fence despite being asked
/// not to; strip a single leading/trailing fence so we serve clean HTML.
fn strip_code_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    // Drop the rest of the opening fence line (e.g. "html").
    let rest = rest.split_once('\n').map(|(_, r)| r).unwrap_or("");
    rest.trim_end().strip_suffix("```").unwrap_or(rest).trim()
}

/// Read a string-valued worker binding off the JS `env` object.
fn env_string(env: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(env, &JsValue::from_str(key))
        .ok()
        .and_then(|v| v.as_string())
        .filter(|s| !s.is_empty())
}

/// The request URL's path (e.g. `/generate`), without query or fragment.
fn path_of(req: &Request) -> Option<String> {
    let url = req.url();
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(&url);
    let path_and_rest = after_scheme.split_once('/').map(|(_, r)| r)?;
    let path = path_and_rest.split(['?', '#']).next().unwrap_or("");
    Some(format!("/{path}"))
}

/// Value of a query-string parameter (`?key=value&...`), percent-decoded.
fn query_param(req: &Request, key: &str) -> Option<String> {
    let url = req.url();
    let (_, query) = url.split_once('?')?;
    let query = query.split('#').next().unwrap_or(query);
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(k) == key {
            let decoded = percent_decode(v);
            return (!decoded.trim().is_empty()).then_some(decoded);
        }
    }
    None
}

/// Minimal `application/x-www-form-urlencoded` decode (`+` -> space, `%XX`).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Extract the hostname from `scheme://host[:port]/...` for the DNS prewarm.
fn host_of(base_url: &str) -> Option<String> {
    let after_scheme = base_url.split_once("://").map(|(_, r)| r).unwrap_or(base_url);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    // Strip a port; leave IPv6 literals (`[::1]`) alone for the common case.
    let host = host.split(':').next().unwrap_or(host);
    (!host.is_empty()).then(|| host.to_string())
}

/// Install the ring-backed rustls `CryptoProvider` as the process default, once.
/// reqwest looks this up when building its rustls client; without it the
/// `rustls-no-provider` build has no crypto and panics.
fn ensure_ring_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Prewarm emscripten's resolution cache for `host` via tokio's async DNS
/// (backed by `emscripten_dns_lookup_async`), so reqwest's subsequent
/// synchronous `getaddrinfo` resolves from cache.
async fn dns_prewarm(host: &str) -> Result<(), String> {
    let _addrs = tokio::net::lookup_host((host, 443))
        .await
        .map_err(|e| format!("dns prewarm for {host:?}: {e}"))?;
    Ok(())
}

fn err_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        out.push_str(&format!(" -> {s}"));
        src = s.source();
    }
    out
}
