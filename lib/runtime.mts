import { isSea, getAsset } from "node:sea";
import { readFileSync, realpathSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

export const isBinary = isSea();
export const cliPath = isBinary
  ? process.execPath
  : fileURLToPath(new URL("../bin/crow.mjs", import.meta.url));

export function version(): string {
  const release = dirname(dirname(cliPath));
  const metadata = join(release, "package.json");
  const raw = isBinary
    ? getAsset("package.json", "utf8")
    : readFileSync(
        existsSync(metadata) ? metadata : join(release, "..", "package.json"),
        "utf8",
      );
  // Source builds keep package metadata one level above dist/bin.
  const parsed: unknown = JSON.parse(raw);
  if (
    !parsed ||
    typeof parsed !== "object" ||
    !("version" in parsed) ||
    typeof parsed.version !== "string"
  )
    throw new Error("Crow version metadata is missing");
  return parsed.version;
}

export function isMain(moduleUrl: string): boolean {
  if (isBinary || !process.argv[1]) return false;
  return moduleUrl === pathToFileURL(realpathSync(process.argv[1])).href;
}

export function inspectionInvocation(sourcePath: string, contextPath?: string) {
  const helper = isBinary
    ? "_inspection-mcp"
    : fileURLToPath(new URL("../bin/inspection-mcp.mjs", import.meta.url));
  return {
    command: process.execPath,
    args: [helper, sourcePath, ...(contextPath ? [contextPath] : [])],
  };
}
