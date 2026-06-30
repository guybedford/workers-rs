# emscripten-goose

A Cloudflare Worker, written in Rust and compiled to
`wasm32-unknown-emscripten`, that uses the [`goose`](https://github.com/aaif-goose/goose)
LLM library to turn a prompt into a web page. The request's **query string is
the prompt**; the worker asks an OpenAI-compatible model to emit an HTML
document and serves it directly:

```
GET /?a neon synthwave landing page for a coffee shop
  -> 200 text/html  (the model's generated page)
```

The point isn't the page — it's that the whole call chain runs on emscripten:

```
goose -> reqwest -> hyper -> tokio::net::TcpStream
         hostname resolution via tokio async DNS (emscripten_dns_lookup_async)
         TLS via rustls + ring
```

None of that works on `wasm32-unknown-unknown`. It works here because of the
patch set below.

## Patch set

This target is bleeding-edge: it depends on unreleased emscripten, a vendored
`wasm-bindgen`, an in-progress tokio port, and a handful of small crate patches.
The workspace `[patch.crates-io]` (repo-root `Cargo.toml`) wires them in. You
need these checked out:

| Dependency | Location | Why |
| --- | --- | --- |
| **emscripten** | `/Users/gbedford/Projects/emscripten` (on `PATH`) | epoll + `emscripten_epoll_set_callback`, async DNS (`emscripten_dns_lookup_async`), and `-sWASM_BINDGEN`. npm deps bootstrapped. |
| **emsdk** | `/Users/gbedford/Projects/emsdk` (`EM_CONFIG`) | LLVM/clang toolchain emcc drives. |
| **wasm-bindgen** | `./wasm-bindgen` (submodule) | The `#[wasm_bindgen(tokio)]` attribute and the emscripten descriptor-interpreter fixes. Patched in for `wasm-bindgen`, `js-sys`, `web-sys`, `wasm-bindgen-futures`, `*-macro-support`, `*-cli-support`. |
| **tokio** | `../tokio` | emscripten event-loop runtime (drives `#[wasm_bindgen(tokio)]` futures) + the async DNS resolver (`ToSocketAddrs`/`lookup_host` over `emscripten_dns_lookup_async`). |
| **socket2** | git `rust-lang/socket2` | emscripten support, not yet released. |
| **ring** | `../ring` | getrandom-backed `SystemRandom` on emscripten (the rustls crypto backend). |
| **libc** | `../libc` | emscripten decls (`pthread_sigmask`/`sigwait`/`faccessat`). |
| **sys-info, fs2, arboard, tree-sitter, tree-sitter-language** | `../*` | small emscripten build/stub fixes (pulled in transitively by goose). |

Paths above are machine-specific (the author's layout); adjust the `[patch]`
entries and the `wrangler.toml` `EM_CONFIG`/`PATH` to your checkouts.

### One-time setup

```sh
# Rust target
rustup target add wasm32-unknown-emscripten

# Build the vendored wasm-bindgen CLI (carries the descriptor-interpreter fixes
# and the #[wasm_bindgen(tokio)] support). emcc invokes `wasm-bindgen` from PATH.
cargo build -p wasm-bindgen-cli --bin wasm-bindgen \
  --manifest-path ../../wasm-bindgen/Cargo.toml
```

> Hostname resolution also requires emscripten's `getaddrinfo` to answer from
> a resolution cache that `emscripten_dns_lookup_async` populates: the worker
> prewarms it with tokio's async DNS, then reqwest's synchronous `GaiResolver`
> reads it. Without that cache, real-hostname `connect`s fail.

## Configure the model (OpenRouter)

The example defaults to [OpenRouter](https://openrouter.ai), which serves free
models. Grab a free API key (no card required) and set it in `wrangler.toml`:

```toml
[vars]
OPENAI_BASE_URL = "https://openrouter.ai/api"   # host up to (not incl.) /v1
OPENAI_MODEL = "meta-llama/llama-3.3-70b-instruct:free"
OPENROUTER_API_KEY = "sk-or-..."
```

For real deployments use a secret instead of a var:

```sh
wrangler secret put OPENROUTER_API_KEY
```

Any OpenAI-compatible endpoint works — point `OPENAI_BASE_URL`/`OPENAI_MODEL`
at Groq, Gemini's OpenAI-compat endpoint, a local server, etc. (`OPENAI_API_KEY`
is accepted as a fallback to `OPENROUTER_API_KEY`.)

## Build & run

The build is a single `cargo build` — rustc drives emcc as the linker and
`-sWASM_BINDGEN=auto` runs wasm-bindgen as a post-link step. `wrangler.toml`'s
`[build]` command is self-contained, so:

```sh
npx wrangler dev      # builds, then serves locally
# open http://localhost:8787/?a%20landing%20page%20for%20a%20bakery
```

To deploy:

```sh
npx wrangler deploy
```

### Local harness (no wrangler)

After a release build, `run.mjs` calls the worker's `fetch` directly:

```sh
OPENROUTER_API_KEY=sk-or-... node run.mjs "a neon synthwave landing page" > page.html
```

## How it works

- `#[wasm_bindgen(tokio)]` on `fetch` drives the returned future on tokio's
  emscripten event-loop runtime (cooperatively, via the host event loop — no
  thread blocking), bridging the outcome to the JS `Promise` the runtime awaits.
- The query string is decoded and used as the prompt, prefixed to ask the model
  for a complete HTML document; the reply is served as `text/html`.
- Before the request, `dns_prewarm` resolves the API host through tokio's async
  DNS to warm emscripten's resolution cache for reqwest's resolver.
- `-sNODERAWSOCKETS` backs the socket layer with node `net`/`dgram`;
  `-fwasm-exceptions` matches the `panic = "unwind"` build tokio's task harness
  relies on.
