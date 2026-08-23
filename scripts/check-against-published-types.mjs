// Compares what openconv-protocol serializes against the published TypeScript.
//
// The Rust fixtures are transcribed by hand from asyncapi-types.ts, and a wrong field
// name there is invisible: it round-trips fine and the client ignores the message. So
// this reads the TypeScript itself and walks each serialized message against the
// interface its `type` names, following declared field types all the way down — the
// transcription is verified rather than trusted.
//
// Not part of `cargo test`: the TypeScript lives in a node_modules tree in another
// repository, which CI does not have. Run it whenever the vendored SDK moves.
//
//   node scripts/check-against-published-types.mjs \
//     ~/code/brandon-fryslie_happy/node_modules/@elevenlabs/types/generated/types/asyncapi-types.ts

import { readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";

const typesPath = process.argv[2];
if (!typesPath) {
  console.error("usage: check-against-published-types.mjs <path to asyncapi-types.ts>");
  process.exit(2);
}

/// Each `export interface X { ... }` as a list of {name, optional, type}. Fields are
/// split on `;` rather than newlines because the generator wraps long union types
/// across several lines.
function parseInterfaces(source) {
  const interfaces = new Map();
  for (const [, name, body] of source.matchAll(/export interface (\w+) \{([^}]*)\}/g)) {
    const fields = [];
    for (const chunk of body.split(";")) {
      const field = chunk.match(/^\s*(\w+)(\??):\s*([\s\S]+)$/);
      if (!field) continue;
      fields.push({
        name: field[1],
        optional: field[2] === "?",
        type: field[3].replace(/\s+/g, " ").trim(),
      });
    }
    interfaces.set(name, fields);
  }
  return interfaces;
}

const interfaces = parseInterfaces(readFileSync(typesPath, "utf8"));

/// The interfaces keyed by the `type` literal they declare. The generator emits a
/// bare and a `*ClientEvent` copy of most messages; identical copies collapse.
const byTag = new Map();
for (const [name, fields] of interfaces) {
  const tag = fields.find((f) => f.name === "type")?.type.match(/^"([^"]+)"$/)?.[1];
  if (!tag) continue;
  const shape = JSON.stringify(fields);
  const existing = byTag.get(tag);
  if (existing && JSON.stringify(existing.fields) !== shape) {
    console.error(`AMBIGUOUS: type "${tag}" declared with two different shapes (${existing.name}, ${name})`);
    process.exit(1);
  }
  byTag.set(tag, { name, fields });
}

/// Walks a serialized value against a declared TypeScript type, collecting every
/// name mismatch it finds. Returns the problems rather than printing them so union
/// alternatives can be tried and discarded.
function checkAgainst(value, type, path) {
  // Unions: the shape is correct if any alternative accepts it whole.
  if (type.startsWith("|") || type.includes(" | ")) {
    const alternatives = type.split("|").map((t) => t.trim()).filter(Boolean);
    if (alternatives.every((t) => /^"[^"]*"$/.test(t) || /^\d+$/.test(t))) return [];
    const attempts = alternatives.map((t) => checkAgainst(value, t, path));
    return attempts.some((problems) => problems.length === 0) ? [] : attempts.flat();
  }

  if (type.endsWith("[]")) {
    const element = type.slice(0, -2);
    if (!Array.isArray(value)) return [`${path}: expected an array`];
    return value.flatMap((item, i) => checkAgainst(item, element, `${path}[${i}]`));
  }

  // Free-form objects and anything that is not an interface (primitives, string
  // unions, numeric literal unions) carry no field names to check.
  const fields = interfaces.get(type);
  if (!fields) return [];

  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    return [`${path}: expected an object shaped like ${type}`];
  }

  const problems = [];
  const declared = new Set(fields.map((f) => f.name));
  for (const key of Object.keys(value)) {
    if (!declared.has(key)) problems.push(`${path}.${key} is not a field of ${type}`);
  }
  for (const field of fields) {
    if (field.name in value) {
      problems.push(...checkAgainst(value[field.name], field.type, `${path}.${field.name}`));
    } else if (!field.optional) {
      problems.push(`${path}.${field.name} is required by ${type} but was not sent`);
    }
  }
  return problems;
}

const serialized = JSON.parse(
  execFileSync("cargo", ["run", "--quiet", "--example", "dump_fixtures"], {
    encoding: "utf8",
    maxBuffer: 32 * 1024 * 1024,
  })
);

const problems = [];
const seen = new Set();

for (const message of serialized) {
  const tag = message.type;
  seen.add(tag);
  const published = byTag.get(tag);
  if (!published) {
    problems.push(`type "${tag}" is not declared anywhere in the published types`);
    continue;
  }
  problems.push(...checkAgainst(message, published.name, tag));
}

// Anything published but never produced is a gap in the port, not necessarily a bug —
// reported separately so it stays a decision rather than a silent omission.
const unported = [...byTag.keys()].filter((tag) => !seen.has(tag));

for (const problem of problems) console.error(`MISMATCH: ${problem}`);
for (const tag of unported) console.warn(`NOT PORTED: "${tag}"`);

if (problems.length > 0) {
  console.error(`\n${problems.length} mismatch(es) against the published types.`);
  process.exit(1);
}

console.log(
  `OK: ${seen.size} message types match the published shapes, fields checked recursively` +
    (unported.length > 0 ? ` (${unported.length} published type(s) not ported)` : "")
);
