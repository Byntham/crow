#!/usr/bin/env node
import { build } from "esbuild";
import { inject } from "postject";
import {
  chmod,
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  realpath,
  rename,
  rm,
  writeFile,
} from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";

const projectRoot = fileURLToPath(new URL("..", import.meta.url));
const pkg = JSON.parse(
  await readFile(join(projectRoot, "package.json"), "utf8"),
);
if (!/^\d+\.\d+\.\d+(?:-[\w.-]+)?$/.test(pkg.version))
  throw new Error("Invalid release version in package.json.");
const args = process.argv.slice(2);
if (args.length && !(args.length === 2 && args[0] === "--out"))
  throw new Error("Usage: node scripts/build-binary.mjs [--out DIRECTORY]");
if (process.platform !== "linux" || !["x64", "arm64"].includes(process.arch))
  throw new Error("Build release binaries natively on Linux x64 or arm64.");
if (Number(process.versions.node.split(".")[0]) !== 24)
  throw new Error(
    "Release binaries require the supported Node 24 LTS runtime.",
  );

const output = resolve(args[1] || join(projectRoot, "dist-release"));
await mkdir(output, { recursive: true });
const stage = await mkdtemp(join(output, ".crow-binary-"));
const run = (command, arguments_) => {
  const result = spawnSync(command, arguments_, { stdio: "inherit" });
  if (result.status !== 0)
    throw (
      result.error ||
      new Error(`${command} failed with status ${result.status}`)
    );
};
try {
  const executable = join(stage, "crow");
  const bundle = join(stage, "crow.cjs");
  await build({
    absWorkingDir: projectRoot,
    stdin: {
      contents:
        'import {main} from "./bin/crow.mts"; main().catch(error => { console.error(`Crow: ${error instanceof Error ? error.message : String(error)}`); process.exitCode = 1; });',
      resolveDir: projectRoot,
      loader: "ts",
    },
    bundle: true,
    platform: "node",
    target: "node24",
    format: "cjs",
    outfile: bundle,
    // Source execution paths are unused in SEA mode, but must remain valid JS.
    define: { "import.meta.url": "__crowBundleUrl" },
    banner: {
      js: 'const __crowBundleUrl = require("node:url").pathToFileURL(__filename).href;',
    },
    logLevel: "warning",
  });
  const blob = join(stage, "crow.blob");
  const config = join(stage, "sea.json");
  await writeFile(
    config,
    JSON.stringify({
      main: bundle,
      output: blob,
      disableExperimentalSEAWarning: true,
      useSnapshot: false,
      useCodeCache: false,
      execArgvExtension: "none",
      assets: { "package.json": join(projectRoot, "package.json") },
    }),
  );
  run(process.execPath, ["--experimental-sea-config", config]);
  await copyFile(process.execPath, executable);
  await chmod(executable, 0o755);
  await inject(executable, "NODE_SEA_BLOB", await readFile(blob), {
    sentinelFuse: "NODE_SEA_FUSE_fce680ab2cc467b6e072b8b5df1996b2",
  });
  const runtimeRoot = dirname(dirname(await realpath(process.execPath)));
  let license;
  try {
    license = await readFile(join(runtimeRoot, "LICENSE"), "utf8");
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
    const response = await fetch(
      `https://raw.githubusercontent.com/nodejs/node/${process.version}/LICENSE`,
      { signal: AbortSignal.timeout(30000) },
    );
    if (!response.ok)
      throw new Error(
        `Could not retrieve Node license: HTTP ${response.status}`,
      );
    license = await response.text();
  }
  if (!license.includes("Permission is hereby granted"))
    throw new Error("The Node distribution license is missing or invalid.");
  await writeFile(
    join(stage, "THIRD_PARTY_NOTICES"),
    `Crow includes the official Node.js ${process.version} runtime.\n\n${license}`,
  );
  const archive = `crow-v${pkg.version}-linux-${process.arch}.tar.gz`;
  run("tar", [
    "-czf",
    join(stage, archive),
    "-C",
    stage,
    "crow",
    "THIRD_PARTY_NOTICES",
  ]);
  const digest = createHash("sha256")
    .update(await readFile(join(stage, archive)))
    .digest("hex");
  await writeFile(join(stage, "SHA256SUMS"), `${digest}  ${archive}\n`);
  for (const file of [archive, "SHA256SUMS"])
    await rename(join(stage, file), join(output, file));
  console.log(`Built ${join(output, archive)} with Node ${process.version}.`);
} finally {
  await rm(stage, { recursive: true, force: true });
}
