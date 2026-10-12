// Smoke test of the Node.js package built by `nemo-wasm/Dockerfile`.
//
//   node nemo-wasm/tests/smoke.mjs [path/to/pkg-node]
//
// Checks that facts added as data give the same results as the same facts
// written as program text, also after replacing facts on a reused program.

import assert from "node:assert/strict";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const pkg = path.resolve(process.argv[2] ?? path.join(here, "..", "pkg-node"));
const { NemoEngine, NemoProgram } = createRequire(import.meta.url)(pkg);

const rules = `
hasEdge(?x) :- edge(?x, ?y).
reach(?x, ?y) :- edge(?x, ?y).
reach(?x, ?z) :- reach(?x, ?y), edge(?y, ?z).
heavy(?x, ?l) :- node(?x, ?l, ?w), ?w > 1.
isolated(?x) :- node(?x, ?l, ?w), ~hasEdge(?x).
tagged(?x) :- node(?x, ?l, ?w), flag(?x, true), kind(?x, special).
@output reach.
@output heavy.
@output isolated.
@output tagged.
`;
const outputs = ["reach", "heavy", "isolated", "tagged"];

function term(value) {
  if (typeof value === "string") return JSON.stringify(value);
  if (typeof value === "object") return value.iri;
  return String(value);
}

function factsAsText(facts) {
  return Object.entries(facts)
    .flatMap(([predicate, rows]) =>
      rows.map((row) => `${predicate}(${row.map(term).join(", ")}).`),
    )
    .join("\n");
}

const canonical = (rows) =>
  rows
    .map((row) => JSON.stringify(row, (_, v) => (typeof v === "bigint" ? `${v}n` : v)))
    .sort();

async function resultsOf(engine) {
  const results = {};
  for (const predicate of outputs) {
    results[predicate] = canonical(await engine.getResultRows(predicate));
  }
  return results;
}

async function viaText(facts) {
  const engine = await NemoEngine.new(rules + factsAsText(facts));
  try {
    await engine.reason();
    return await resultsOf(engine);
  } finally {
    engine.free();
  }
}

const facts = {
  edge: [["a", "b"], ["b", "c"], ["c", "a"], ["d", "e"]],
  node: [
    ["a", "first", 1],
    ["b", "second", 2.5],
    ["f", "lonely", 3],
    ["g", "big", 12345678901],
  ],
  flag: [["a", true], ["f", false], ["g", true]],
  kind: [["a", { iri: "special" }], ["g", { iri: "special" }], ["f", "special"]],
};

// Prepare the rules once, add facts as rows.
const program = new NemoProgram(rules);
const engine = await program.createEngine();
for (const [predicate, rows] of Object.entries(facts)) {
  engine.addFacts(predicate, rows);
}
await engine.reason();
const fromRows = await resultsOf(engine);
assert.deepEqual(fromRows, await viaText(facts));
assert.equal(fromRows.reach.length, 10);
assert.deepEqual(fromRows.isolated, canonical([["f"], ["g"]]));
assert.deepEqual(fromRows.tagged, canonical([["a"], ["g"]]));

// Results are iterable.
const iterated = [];
for (const row of await engine.getResult("heavy")) {
  assert.equal(typeof row.id, "number");
  iterated.push(row.values);
}
assert.deepEqual(canonical(iterated), fromRows.heavy);

// Replace one predicate's facts and reason again, without parsing the rules.
const replaced = { ...facts, edge: [["f", "a"], ["a", "g"]] };
engine.setFacts("edge", replaced.edge);
await engine.reason();
assert.deepEqual(await resultsOf(engine), await viaText(replaced));

// Adding facts after reasoning also starts over.
const extended = { ...replaced, edge: [...replaced.edge, ["g", "b"]] };
engine.addFacts("edge", [["g", "b"]]);
await engine.reason();
assert.deepEqual(await resultsOf(engine), await viaText(extended));

// Invalid rows are rejected.
assert.throws(() => engine.addFacts("edge", [["a"]]), /arity 2/);
assert.throws(() => engine.addFacts("edge", [["a", {}]]), /expected a string/);
assert.equal(typeof engine[Symbol.dispose], "function");
engine.free();

// Imports read resources given as Uint8Array, string or Blob, outside of a worker.
const importing = new NemoProgram(`
@import edge :- csv{resource="edges.csv"}.
@import more :- csv{resource="more.csv"}.
@import blob :- csv{resource="blob.csv"}.
all(?x, ?y) :- edge(?x, ?y).
all(?x, ?y) :- more(?x, ?y).
all(?x, ?y) :- blob(?x, ?y).
@output all.
`);
const withImports = await importing.createEngine({
  "edges.csv": new TextEncoder().encode("a,b\nb,c\n"),
  "more.csv": "c,d\n",
  "blob.csv": new Blob(["d,e\n"]),
});
await withImports.reason();
assert.equal((await withImports.getResultRows("all")).length, 4);
withImports.free();
importing.free();

// Many engines from one program, freed with `using`, stay within memory.
for (let i = 0; i < 300; i++) {
  using many = await program.createEngine();
  many.addFacts("edge", facts.edge);
  await many.reason();
}

// Timing: text facts parsed per engine against a prepared program with rows.
const size = 2000;
const big = {
  edge: Array.from({ length: size }, (_, i) => [`n${i}`, `n${(i * 7 + 1) % size}`]),
  node: Array.from({ length: size }, (_, i) => [`n${i}`, `label${i % 10}`, i % 3]),
};
const time = async (label, runs, fn) => {
  await fn();
  const start = performance.now();
  for (let i = 0; i < runs; i++) await fn();
  console.log(`${label}: ${((performance.now() - start) / runs).toFixed(1)} ms`);
};
await time(`text, ${size * 2} facts (NemoEngine.new + reason + read)`, 3, async () => {
  using e = await NemoEngine.new(rules + factsAsText(big));
  await e.reason();
  await e.getResultRows("heavy");
});
await time(`prepared + rows, ${size * 2} facts (createEngine + addFacts + reason + read)`, 3, async () => {
  using e = await program.createEngine();
  e.addFacts("edge", big.edge);
  e.addFacts("node", big.node);
  await e.reason();
  await e.getResultRows("heavy");
});
program.free();

console.log("nemo-wasm smoke test passed");
