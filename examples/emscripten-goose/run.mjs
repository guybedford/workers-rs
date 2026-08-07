// Local harness: build first (see README), then run e.g.
//   CLOUDFLARE_ACCOUNT_ID=... CLOUDFLARE_API_TOKEN=... \
//     node run.mjs "a neon synthwave landing page"
//
// It imports the emscripten factory, calls the worker's `fetch` with the
// prompt as the query string and the AI Gateway config as the `env` bindings,
// and writes the returned HTML to stdout (redirect to a file to view it).
//
// The build emits an ESM module; node only treats it as ESM with a `.mjs`
// extension, so we import a `.mjs` copy of the cargo output.
import { copyFileSync } from "node:fs";

const OUT = process.env.EMSCRIPTEN_GOOSE_OUT ??
  "../../target/wasm32-unknown-emscripten/release/emscripten-goose";
copyFileSync(`${OUT}.js`, `${OUT}.mjs`);
const { default: worker } = await import(`${OUT}.mjs`);

const env = {
  CLOUDFLARE_ACCOUNT_ID: process.env.CLOUDFLARE_ACCOUNT_ID ?? "",
  CF_AIG_GATEWAY_ID: process.env.CF_AIG_GATEWAY_ID ?? "",
  OPENAI_BASE_URL: process.env.OPENAI_BASE_URL ?? "",
  OPENAI_MODEL: process.env.OPENAI_MODEL ?? "openai/gpt-4.1",
  CLOUDFLARE_API_TOKEN: process.env.CLOUDFLARE_API_TOKEN ?? process.env.OPENAI_API_KEY ?? "",
};

const prompt = process.argv.slice(2).join(" ") || "a friendly hello-world landing page";
const url = `http://localhost/?${encodeURIComponent(prompt)}`;

const res = await worker.fetch(new Request(url), env, {});
process.stderr.write(`status: ${res.status}\n`);
process.stdout.write(await res.text());
process.exit(res.status === 200 ? 0 : 1);
