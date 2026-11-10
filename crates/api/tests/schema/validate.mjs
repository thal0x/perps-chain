// Usage: node validate.mjs <samples.json> [sdkRoot]
// samples: [{kind: "info"|"sub"|"exchange", method: "<file stem>", label, data}]
// Each sample is validated against the response/event type exported by
// <sdkRoot>/src/api/<dir>/_methods/<method>.ts, converted to JSON Schema
// with the same generator settings the SDK's tests use (additionalProperties:
// false, so unknown fields are errors too).
import fs from "node:fs";
import path from "node:path";
import { createGenerator } from "ts-json-schema-generator";
import ts from "typescript";
import Ajv from "ajv";

const [samplesPath, sdkArg] = process.argv.slice(2);
const sdkRoot = sdkArg || process.env.HL_TS_SDK || "/home/user/ext/hyperliquid";
const samples = JSON.parse(fs.readFileSync(samplesPath, "utf8"));
const dirs = { info: "info", sub: "subscription", exchange: "exchange" };
const suffix = { info: "Response", sub: "Event", exchange: "Response" };

const compilerOptions = {
  target: ts.ScriptTarget.ESNext,
  module: ts.ModuleKind.ESNext,
  moduleResolution: ts.ModuleResolutionKind.Bundler,
  strict: true,
  noEmit: true,
  allowImportingTsExtensions: true,
  skipLibCheck: true,
};
const ajv = new Ajv({ strict: false, allErrors: true });
const cache = new Map();

function validatorFor(kind, method) {
  const key = `${kind}:${method}`;
  if (cache.has(key)) return cache.get(key);
  const file = path.join(sdkRoot, "src/api", dirs[kind], "_methods", `${method}.ts`);
  const src = fs.readFileSync(file, "utf8");
  const re = new RegExp(`export type (\\w+${suffix[kind]}) =`);
  const m = src.match(re);
  if (!m) throw new Error(`no ${suffix[kind]} type in ${file}`);
  const program = ts.createProgram([file], compilerOptions);
  const gen = createGenerator({
    skipTypeCheck: true,
    jsDoc: "extended",
    sortProps: false,
    strictTuples: true,
    additionalProperties: false,
    topRef: true,
    expose: "all",
    tsProgram: program,
    path: file,
  });
  const schema = gen.createSchema(m[1]);
  const v = { name: m[1], validate: ajv.compile(schema) };
  cache.set(key, v);
  return v;
}

let failed = 0;
const seen = new Set();
for (const s of samples) {
  const { name, validate } = validatorFor(s.kind, s.method);
  seen.add(`${s.kind}:${s.method}`);
  if (!validate(s.data)) {
    failed++;
    console.log(`FAIL ${s.kind}:${s.method} (${name}) [${s.label}]`);
    for (const e of validate.errors.slice(0, 8)) {
      console.log(`   ${e.instancePath || "/"} ${e.message} ${JSON.stringify(e.params)}`);
    }
    console.log(`   data: ${JSON.stringify(s.data).slice(0, 600)}`);
  }
}
console.log(`${samples.length - failed}/${samples.length} samples valid; ${seen.size} distinct SDK types checked`);
process.exit(failed ? 1 : 0);
