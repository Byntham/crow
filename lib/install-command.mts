import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { lstat } from "node:fs/promises";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline/promises";
import { binaryPath, installBinary } from "./binary-install.mjs";
import { acquireLock, errorCode, processRun, hostEnv } from "./util.mjs";
import { isBinary, version } from "./runtime.mjs";

async function checksum(file: string) {
  const hash = createHash("sha256");
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest("hex");
}

// Initial installation and repeated onboarding must not bypass crow update's
// drain, version selection, or rollback by switching an existing executable.
export async function installDownloaded(
  root: string,
  options: Parameters<typeof installBinary>[1],
) {
  root = resolve(root);
  const release = await acquireLock(join(root, "install.lock"));
  try {
    let currentExists = false;
    try {
      await lstat(join(root, "current"));
      currentExists = true;
    } catch (error) {
      if (errorCode(error) !== "ENOENT") throw error;
    }
    if (currentExists) {
      let same = false;
      try {
        same =
          (await checksum(binaryPath(root))) ===
          (await checksum(options.executable ?? process.execPath));
      } catch (error) {
        if (errorCode(error) !== "ENOENT") throw error;
      }
      if (!same)
        throw new Error(
          "A different Crow installation already exists. Use its crow update command to upgrade, or its crow setup command to continue onboarding. The installer has left it unchanged.",
        );
    }
    return await installBinary(root, options);
  } finally {
    await release();
  }
}

export async function installCommand(root: string, noSetup: boolean) {
  if (!isBinary)
    throw new Error(
      "crow install requires the standalone binary. For source installation, use scripts/install.sh.",
    );
  if (process.getuid?.() === 0)
    throw new Error(
      "Run the Crow installer as your normal user, without sudo.",
    );
  const executable = await installDownloaded(root, { version: version() });
  console.log(`Installed Crow ${version()} at ${executable}`);
  if (
    !(process.env.PATH || "").split(":").includes(join(homedir(), ".local/bin"))
  )
    console.log(
      'To use crow in this shell, run: export PATH="$HOME/.local/bin:$PATH"',
    );
  const quote = (value: string) => `'${value.replaceAll("'", "'\"'\"'")}'`;
  const setupCommand = `CROW_HOME=${quote(resolve(root))} ${quote(executable)} setup`;
  if (noSetup || !process.stdin.isTTY) {
    console.log(`Start or continue setup with: ${setupCommand}`);
    return;
  }
  const terminal = createInterface({
    input: process.stdin,
    output: process.stdout,
  });
  let answer: string;
  try {
    answer = (await terminal.question("Start Crow setup now? [Y/n]: "))
      .trim()
      .toLowerCase();
  } finally {
    terminal.close();
  }
  if (answer && answer !== "y" && answer !== "yes") {
    console.log(`Start or continue setup with: ${setupCommand}`);
    return;
  }
  await processRun(executable, ["setup"], {
    inherit: true,
    detached: false,
    env: hostEnv({ CROW_HOME: resolve(root) }),
  });
}
