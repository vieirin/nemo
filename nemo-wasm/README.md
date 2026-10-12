# nemo-wasm

This crate provides a Web Assembly build and JavaScript/TypeScript bindings for the `nemo` crate.

> **Note**
> These bindings are currently in an experimental state and subject to change.

## Building

- Install [wasm-pack](https://rustwasm.github.io/wasm-pack/book/prerequisites/index.html)
- Build the library:

```
wasm-pack build --out-dir nemoWASMBundler --target bundler --weak-refs --release
wasm-pack build --out-dir nemoWASMWeb --target web --weak-refs --release
```

- The language server (`createNemoLanguageServer`) is behind the default `language-server` feature. To build only the engine, which is smaller, pass `-- --no-default-features --features web_sys_unstable_apis` to `wasm-pack build`.
- In order to use the `FileSystemSyncAccessHandle` APIs, the `web_sys_unstable_apis` `cfg` flag needs to be set
  - See https://rustwasm.github.io/docs/wasm-bindgen/web-sys/unstable-apis.html
  - See https://rustwasm.github.io/wasm-bindgen/api/web_sys/struct.FileSystemSyncAccessHandle.html

### In Docker

`Dockerfile` builds the packages with a dated nightly and a pinned `wasm-pack`, without a local Rust toolchain. From the repository root:

```
docker build -f nemo-wasm/Dockerfile --output type=local,dest=nemo-wasm .
node nemo-wasm/tests/smoke.mjs
```

This writes `nemo-wasm/pkg-node` (`--target nodejs`) and `nemo-wasm/pkg-web` (`--target web`), without the language server (add `--build-arg FEATURES=language-server` to include it), and runs a smoke test against the Node.js package.

### Releases

Packages built this way are attached to the fork's GitHub releases, so they can be installed by URL without building anything:

```
node nemo-wasm/pack.mjs pkg-node 0.10.1-fork.1 dist   # writes dist/nemo-wasm-node-0.10.1-fork.1.tgz
node nemo-wasm/pack.mjs pkg-web 0.10.1-fork.1 dist    # writes dist/nemo-wasm-web-0.10.1-fork.1.tgz
```

The packages keep the name `nemo-wasm`. Depend on a release asset by its URL, for example:

```json
"nemo-wasm": "https://github.com/vieirin/nemo/releases/download/nemo-wasm-v0.10.1-fork.1/nemo-wasm-node-0.10.1-fork.1.tgz"
```

## Example usage

```typescript
import { NemoEngine, NemoProgram } from "nemo-wasm";

// Parse, validate, transform and plan the rules once.
const program = new NemoProgram(`
reach(?x, ?y) :- edge(?x, ?y).
reach(?x, ?z) :- reach(?x, ?y), edge(?y, ?z).
@output reach.
`);

// Engines created from the program don't parse the rules again.
// `using` frees the engine at the end of the block (or call `engine.free()`).
{
  using engine = await program.createEngine();

  // Facts as data: strings are plain strings ("a" in Nemo), safe integers and
  // bigints are integers, other numbers are doubles, `{ iri: "a" }` is the constant a.
  engine.addFacts("edge", [["a", "b"], ["b", "c"]]);
  await engine.reason();
  console.log(await engine.getResultRows("reach"));

  // Replace a predicate's facts and reason again: this starts over
  // from the prepared program, without parsing anything.
  engine.setFacts("edge", [["c", "d"]]);
  await engine.reason();

  for (const row of await engine.getResult("reach")) {
    console.log(row.values, row.id);
  }
}

// Programs with facts and imports can also be given as text. Resources for
// imports may be Blobs, Uint8Arrays, ArrayBuffers or strings.
const engine = await NemoEngine.new(
  `@import edge :- csv{resource="edges.csv"}. @output edge.`,
  { "edges.csv": "a,b\n" },
);
await engine.reason();
engine.free();
```

Engines, programs and results hold WebAssembly memory. Free them with `free()` or `using` when they are no longer needed: the garbage collector does not notice how much memory they hold, and creating many engines without freeing them can exhaust the WebAssembly memory.
