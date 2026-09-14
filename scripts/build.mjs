#!/usr/bin/env node
// Node strips erasable TypeScript syntax; pnpm check supplies strict type checking.
import { stripTypeScriptTypes } from "node:module";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rename,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { dirname, join, resolve, sep } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { spawnSync } from "node:child_process";

export const projectRoot = fileURLToPath(new URL("..", import.meta.url));

export async function build({ sourceRoot = projectRoot, outDir } = {}) {
  const source = resolve(sourceRoot);
  const output = resolve(outDir || join(source, "dist"));
  const protectedDirectories = [
    source,
    ...["bin", "lib", "scripts", "test", "docs"].map((name) =>
      join(source, name),
    ),
  ];
  if (
    protectedDirectories.some(
      (directory) =>
        directory === output || directory.startsWith(`${output}${sep}`),
    ) ||
    ["bin", "lib", "scripts", "test", "docs"].some((name) =>
      output.startsWith(`${join(source, name)}${sep}`),
    )
  )
    throw new Error(
      "Build output must be separate from Crow source directories.",
    );
  await mkdir(dirname(output), { recursive: true });
  const stage = await mkdtemp(join(dirname(output), ".crow-build-"));
  let modules = 0;
  try {
    async function compile(directory, target) {
      await mkdir(target, { recursive: true });
      for (const entry of await readdir(directory, { withFileTypes: true })) {
        const input = join(directory, entry.name);
        if (entry.isDirectory()) {
          await compile(input, join(target, entry.name));
          continue;
        }
        if (!entry.isFile())
          throw new Error(`Unsupported linked build input: ${input}`);
        if (entry.name.endsWith(".d.mts")) continue;
        if (!entry.name.endsWith(".mts"))
          throw new Error(`Expected a TypeScript .mts source file: ${input}`);
        const emitted = join(target, entry.name.replace(/\.mts$/, ".mjs"));
        const code = stripTypeScriptTypes(await readFile(input, "utf8"), {
          mode: "strip",
        });
        await writeFile(emitted, code);
        await chmod(emitted, (await stat(input)).mode & 0o777);
        const check = spawnSync(process.execPath, ["--check", emitted], {
          encoding: "utf8",
        });
        if (check.status !== 0)
          throw new Error(
            `Invalid emitted JavaScript for ${input}: ${check.stderr || check.error?.message || "syntax check failed"}`,
          );
        modules++;
      }
    }
    for (const directory of ["bin", "lib"])
      await compile(join(source, directory), join(stage, directory));
    if (!modules)
      throw new Error("Crow build did not find any TypeScript modules.");
    await rm(output, { recursive: true, force: true });
    await rename(stage, output);
    return { output, modules };
  } finally {
    await rm(stage, { recursive: true, force: true });
  }
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(resolve(process.argv[1])).href
) {
  const args = process.argv.slice(2);
  if (args.length && !(args.length === 2 && args[0] === "--out")) {
    console.error("Usage: node scripts/build.mjs [--out DIRECTORY]");
    process.exitCode = 1;
  } else {
    try {
      const result = await build({ outDir: args[1] });
      console.log(`Built ${result.modules} modules in ${result.output}`);
    } catch (error) {
      console.error(error.message);
      process.exitCode = 1;
    }
  }
}
