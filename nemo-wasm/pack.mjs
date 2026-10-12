// Pack a package built by `Dockerfile` into a tarball that can be attached to
// a release of this fork and installed by URL with npm or pnpm.
//
//   node nemo-wasm/pack.mjs <pkg-node|pkg-web> <version> [out-dir]
//
// The package keeps the name `nemo-wasm`; its version is set to <version>
// (for example `0.10.1-fork.1`) and its repository to this fork.

import { cpSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { execFileSync } from "node:child_process";

const [pkg, version, outDir = "."] = process.argv.slice(2);
if (!["pkg-node", "pkg-web"].includes(pkg) || !version) {
  console.error("usage: node nemo-wasm/pack.mjs <pkg-node|pkg-web> <version> [out-dir]");
  process.exit(1);
}

const source = new URL(pkg, import.meta.url).pathname;
const staging = mkdtempSync(join(tmpdir(), "nemo-wasm-pack-"));
try {
  cpSync(source, staging, { recursive: true });

  const manifestPath = join(staging, "package.json");
  const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  manifest.version = version;
  manifest.repository = { type: "git", url: "https://github.com/vieirin/nemo" };
  manifest.homepage = "https://github.com/vieirin/nemo/tree/wasm-fast-reevaluation/nemo-wasm";
  writeFileSync(manifestPath, JSON.stringify(manifest, null, 2) + "\n");

  const tarball = execFileSync("npm", ["pack", "--pack-destination", resolve(outDir)], {
    cwd: staging,
    encoding: "utf8",
  }).trim();

  // npm names the tarball after the package; tell the targets apart.
  const target = pkg === "pkg-node" ? "node" : "web";
  const renamed = `nemo-wasm-${target}-${version}.tgz`;
  cpSync(join(resolve(outDir), tarball), join(resolve(outDir), renamed));
  rmSync(join(resolve(outDir), tarball));
  console.log(join(resolve(outDir), renamed));
} finally {
  rmSync(staging, { recursive: true, force: true });
}
