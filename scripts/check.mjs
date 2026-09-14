import { readdir } from "node:fs/promises";
import { spawnSync } from "node:child_process";
import { join } from "node:path";
import { build, projectRoot } from "./build.mjs";

// This dependency-free check also runs during crow update. pnpm check runs the
// strict TypeScript check first; installed users do not need a compiler or pnpm.
if (process.argv.slice(2).some((argument) => argument !== "--runtime-only")) {
  throw new Error("Usage: node scripts/check.mjs [--runtime-only]");
}
await build();
for (const file of await readdir(join(projectRoot, "scripts"))) {
  if (!file.endsWith(".mjs")) continue;
  const result = spawnSync(
    process.execPath,
    ["--check", join(projectRoot, "scripts", file)],
    { stdio: "inherit" },
  );
  if (result.status !== 0) process.exit(result.status ?? 1);
}
const tests = spawnSync(
  process.execPath,
  [
    "--test",
    ...(await readdir(join(projectRoot, "test")))
      .filter((file) => file.endsWith(".test.mjs"))
      .map((file) => join(projectRoot, "test", file)),
  ],
  { cwd: projectRoot, stdio: "inherit" },
);
process.exit(tests.status ?? 1);
