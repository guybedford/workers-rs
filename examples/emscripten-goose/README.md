# Emscripten Goose Example

[`goose`](https://github.com/aaif-goose/goose) running on `wasm32-unknown-emscripten`
on Cloudflare Workers using AI Gateway.

## Setup Instructions with Patchset

1. Ensure latest Rust toolchain installed with Emscripten target via [Rustup]:

```
rustup install stable
rustup default stable
rustup target add wasm32-unknown-emscripten
```

2. Install Emscripten with patches

```sh
# Emsdk
git clone https://github.com/emscripten-core/emsdk
cd emsdk
./emsdk install latest
./emsdk activate latest

# Emscripten Patchset
git clone -b cf https://github.com/guybedford/emscripten
( cd emscripten && npm install )
```

3. Clone & Build workers-rs with Submodule Patches

```
git clone --recurse-submodules -b emscripten-layering https://github.com/guybedford/workers-rs
cd workers-rs
npm run build
```

All patched dependencies are pinned as submodules on public branches (see the
patch-set table below), so the recursive clone is the whole setup.

4. Setup the API keys

Edit `examples/emscripten-goose/wrangler.toml` and set:

* `CLOUDFLARE_ACCOUNT_ID`: your Cloudflare account ID
* `CLOUDFLARE_API_TOKEN`: your API token for Workers AI
* `OPENAI_MODEL`: the model to use


5. Run the example repo

```
cd examples/emscripten-goose
npx wrangler@latest dev
```

Then navigate to `localhost:5776` once build completes.

> Note WARP needs to be disabled currently for Cloudflare internal testing.

## Patch set

Patches include:

| Dependency | Location | Why |
| --- | --- | --- |
| **emscripten** | sibling checkout `../../../emscripten` (fork `guybedford/emscripten`, branch `cf`) | epoll + `emscripten_epoll_set_callback`, async DNS (`emscripten_dns_lookup_async`), and `-sWASM_BINDGEN`. |
| **emsdk** | sibling checkout `../../../emsdk` | LLVM/clang toolchain emcc drives. |
| **wasm-bindgen** | submodule `wasm-bindgen/` (fork `guybedford/wasm-bindgen`, branch `emscripten-tokio-attribute`) | The `#[wasm_bindgen(tokio)]` attribute and the emscripten descriptor-interpreter fixes (also `js-sys`, `web-sys`, `wasm-bindgen-futures`, `*-macro-support`, `*-cli-support`). |
| **tokio** | submodule `tokio/` (fork `guybedford/tokio`, branch `emscripten-layering-minimal`) | emscripten hosted event-loop runtime (`HostedRuntime`, drives `#[wasm_bindgen(tokio)]` futures) + async DNS resolver (`lookup_host` over `emscripten_dns_lookup_async`). |
| **socket2** | git `rust-lang/socket2` | emscripten support, not yet released. |
| **ring** | submodule `ring/` | getrandom-backed `SystemRandom` on emscripten (the rustls crypto backend). |
| **libc** | submodule `libc/` | emscripten decls (`pthread_sigmask`/`sigwait`/`faccessat`). |
| **sys-info, fs2, arboard, tree-sitter** | submodules | small emscripten build/stub fixes (pulled in transitively by goose). |

The layout the build expects is three checkouts side by side:

```
<parent>/
  emsdk/
  emscripten/
  workers-rs/          <- run wrangler dev from examples/emscripten-goose
```

`wrangler.toml`'s `[build]` defaults `EMSDK`/`EMSCRIPTEN` to `../../../emsdk`
and `../../../emscripten` (relative to the example dir); export either to
override for a different layout.


## Configure the model (AI Gateway)

The worker calls the AI Gateway OpenAI-compatible (`/compat`) endpoint. The base
URL is derived from your account id and gateway name as
`https://gateway.ai.cloudflare.com/v1/<account>/<gateway>/compat`. Set these in
`wrangler.toml` (`wrangler whoami` prints your account id):

```toml
[vars]
CLOUDFLARE_ACCOUNT_ID = "your-account-id"
CF_AIG_GATEWAY_ID = "your-gateway-name"
OPENAI_MODEL = "workers-ai/@cf/meta/llama-3.3-70b-instruct-fp8-fast"
CLOUDFLARE_API_TOKEN = "..."   # token with Workers AI / AI Gateway access
```

For real deployments use a secret instead of an in-file var:

```sh
wrangler secret put CLOUDFLARE_API_TOKEN
```

`CLOUDFLARE_API_TOKEN` is sent as the bearer token. For Workers AI models
(`workers-ai/@cf/...`) it authenticates you to Workers AI; for third-party
models (`openai/gpt-4.1`, `anthropic/claude-sonnet-4-5`, …) the gateway forwards
it to that provider, so it must be *that provider's* key. `OPENAI_API_KEY` is
accepted as a fallback, and `OPENAI_BASE_URL` overrides the derived URL (give
the host up to, not including, `chat/completions` — i.e. ending in `/compat`).

> **WARP gotcha:** Cloudflare WARP intercepts `gateway.ai.cloudflare.com` and
> the worker's `connect` fails with a network error. Disconnect WARP
> (`warp-cli disconnect`) while running the example.

## Build & run

The build is a single `cargo build` — rustc drives emcc as the linker and
`-sWASM_BINDGEN=auto` runs wasm-bindgen as a post-link step. `wrangler.toml`'s
`[build]` command is self-contained, so:

```sh
cd workers-rs/examples/emscripten-goose
npx wrangler dev      # builds, then serves locally
# open http://localhost:8787/
```

To deploy:

```sh
npx wrangler deploy
```

### Local harness (no wrangler)

After a release build, `run.mjs` calls the worker's `fetch` directly:

```sh
CLOUDFLARE_ACCOUNT_ID=... CF_AIG_GATEWAY_ID=... CLOUDFLARE_API_TOKEN=... \
  node run.mjs "a neon synthwave landing page" > page.html
```

## How it works

- `#[wasm_bindgen(tokio)]` on `fetch` schedules the returned future onto the
  ambient tokio hosted event-loop runtime (cooperatively, via the host event
  loop — no thread blocking), bridging the outcome to the JS `Promise` the
  runtime awaits. The ambient runtime lives in `wasm-bindgen-futures` behind
  its `tokio` feature — crates using the attribute must enable it (see this
  example's `Cargo.toml`) — and is shared by all `tokio` exports on the
  thread, so timers, sockets, and `tokio::spawn` from any of them run on one
  runtime.
- Any path other than `/generate` serves the landing form; `/generate` reads the
  `prompt` query param, prefixes it to ask the model for a complete HTML
  document, and serves the reply as `text/html`. The system prompt requires
  inline CSS and inline images (`data:`/SVG), and `max_tokens` is raised so
  richer pages don't truncate. Streaming is disabled because Workers AI's compat
  stream can emit a non-string `delta.content` that goose's parser rejects.
- Before the request, `dns_prewarm` resolves the gateway host through tokio's
  async DNS to warm emscripten's resolution cache for reqwest's resolver.
- `-sNODERAWSOCKETS` backs the socket layer with node `net`/`dgram`;
  `-fwasm-exceptions` matches the `panic = "unwind"` build tokio's task harness
  relies on.
