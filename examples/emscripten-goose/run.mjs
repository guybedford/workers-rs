// Local harness: build first (see README), then run e.g.
//   OPENROUTER_API_KEY=sk-or-... node run.mjs "a neon synthwave landing page"
//
// It imports the emscripten factory, calls the worker's `fetch` with the
// prompt as the query string and the OpenRouter config as the `env` bindings,
// and writes the returned HTML to stdout (redirect to a file to view it).
//
// The build emits an ESM module; node only treats it as ESM with a `.mjs`
// extension, so we import a `.mjs` copy of the cargo output.
import { copyFileSync } from "node:fs";

const OUT = "../../target/wasm32-unknown-emscripten/release/emscripten-goose";
copyFileSync(`${OUT}.js`, `${OUT}.mjs`);
const { default: Module } = await import(`${OUT}.mjs`);

const env = {
  OPENAI_BASE_URL: process.env.OPENAI_BASE_URL ?? "https://openrouter.ai/api",
  OPENAI_MODEL: process.env.OPENAI_MODEL ?? "meta-llama/llama-3.3-70b-instruct:free",
  OPENROUTER_API_KEY: process.env.OPENROUTER_API_KEY ?? process.env.OPENAI_API_KEY ?? "",
};

const prompt = process.argv.slice(2).join(" ") || "a friendly hello-world landing page";
const url = `http://localhost/?${encodeURIComponent(prompt)}`;

const m = await Module();
const res = await m.fetch(new Request(url), env, {});
process.stderr.write(`status: ${res.status}\n`);
process.stdout.write(await res.text());
process.exit(res.status === 200 ? 0 : 1);
