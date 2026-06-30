//! A Cloudflare Worker, written in Rust and compiled to
//! `wasm32-unknown-emscripten`, that uses the [`goose`] LLM library to turn a
//! prompt into a web page. The request's query string *is* the prompt; the
//! worker asks an OpenAI-compatible model (OpenRouter by default) to emit an
//! HTML document and serves the result directly.
//!
//! The point of the example is that this works at all on emscripten: the call
//! goes goose -> reqwest -> hyper -> tokio `TcpStream`, with hostname resolution
//! via tokio's async DNS and TLS via rustls + ring, none of which is available
//! on `wasm32-unknown-unknown`.
//!
//! Configure via worker vars/secrets (see `wrangler.toml`):
//!   * `OPENROUTER_API_KEY` (or `OPENAI_API_KEY`) — required for OpenRouter.
//!   * `OPENAI_BASE_URL` — host up to (not including) `/v1`; default OpenRouter.
//!   * `OPENAI_MODEL` — any model the endpoint serves; default a free one.

use wasm_bindgen::prelude::*;
use web_sys::{Request, Response, ResponseInit};

// `main` runs automatically on init (the emscripten idiom); the worker entry
// point is the `fetch` export below.
fn main() {}

// OpenRouter's base is given as the host up to (not including) `/v1`: goose's
// `OpenAiProvider` joins its default `v1/chat/completions` path onto it.
const DEFAULT_BASE_URL: &str = "https://openrouter.ai/api";
const DEFAULT_MODEL: &str = "meta-llama/llama-3.3-70b-instruct:free";
const DEFAULT_PROMPT: &str = "a friendly hello-world landing page";

const SYSTEM_PROMPT: &str = "You are a web page generator. Reply with a single, complete, \
self-contained HTML document and nothing else: no commentary, no markdown code fences.";
const PROMPT_PREFIX: &str = "Output an HTML file for the following request: ";

#[wasm_bindgen(tokio, js_namespace = ["default"])]
pub async fn fetch(request: Request, env: JsValue, _ctx: JsValue) -> Result<Response, JsValue> {
    std::panic::set_hook(Box::new(|info| {
        web_sys::console::error_1(&format!("RUST PANIC: {info}").into());
    }));

    let prompt = query_string(&request).unwrap_or_else(|| DEFAULT_PROMPT.to_string());
    let base_url = env_string(&env, "OPENAI_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.into());
    let model = env_string(&env, "OPENAI_MODEL").unwrap_or_else(|| DEFAULT_MODEL.into());
    let api_key =
        env_string(&env, "OPENROUTER_API_KEY").or_else(|| env_string(&env, "OPENAI_API_KEY"));

    let (status, content_type, body) =
        match generate_page(&base_url, &model, api_key.as_deref(), &prompt).await {
            Ok(html) => (200, "text/html; charset=utf-8", html),
            Err(e) => (502, "text/plain; charset=utf-8", format!("error: {e}")),
        };

    let init = ResponseInit::new();
    init.set_status(status);
    let headers = web_sys::Headers::new()?;
    headers.set("content-type", content_type)?;
    init.set_headers(&headers);
    Response::new_with_opt_str_and_init(Some(&body), &init)
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
    use goose::providers::openai::OpenAiProvider;
    use goose_providers::model::ModelConfig;

    ensure_ring_provider();

    // reqwest's `GaiResolver` runs `getaddrinfo` on the blocking pool, which on
    // emscripten only answers from the resolution cache; tokio's async DNS
    // populates that cache without blocking the host event loop.
    if let Some(host) = host_of(base_url) {
        dns_prewarm(&host).await?;
    }

    let auth = AuthMethod::BearerToken(api_key.unwrap_or_default().to_string());
    let client = ApiClient::new_with_tls(base_url.to_string(), auth, None)
        .map_err(|e| format!("ApiClient::new_with_tls: {e}"))?;
    let model_cfg = ModelConfig::new(model);
    let provider = OpenAiProvider::new(client);

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

/// The request's query string, percent-decoded, used verbatim as the prompt.
fn query_string(req: &Request) -> Option<String> {
    let url = req.url();
    let (_, query) = url.split_once('?')?;
    let decoded = percent_decode(query);
    (!decoded.trim().is_empty()).then_some(decoded)
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
