import { randomBytes, createHash, timingSafeEqual } from "node:crypto";
import { mkdir, open, unlink, rename, readFile } from "node:fs/promises";
import { dirname } from "node:path";
import { spawn } from "node:child_process";
export interface ProcessOptions {
  cwd?: string;
  env?: NodeJS.ProcessEnv;
  input?: string;
  signal?: AbortSignal;
  timeout?: number;
  limit?: number;
  onLine?: (line: string) => void;
  onChunk?: (chunk: string) => void;
  inherit?: boolean;
  detached?: boolean;
  capture?: boolean;
}
export interface ProcessResult {
  stdout: string;
  stderr: string;
}
export type ProcessRunner = (
  command: string,
  args: string[],
  options?: ProcessOptions,
) => Promise<ProcessResult>;
export function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}
export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
export function errorCode(error: unknown): unknown {
  return isRecord(error) ? error.code : undefined;
}
export const id = () => randomBytes(16).toString("hex");
export const hash = (value: unknown) =>
  createHash("sha256")
    .update(typeof value === "string" ? value : JSON.stringify(value))
    .digest("hex");
export const equal = (a: unknown, b: unknown) =>
  typeof a === "string" &&
  typeof b === "string" &&
  Buffer.byteLength(a) === Buffer.byteLength(b) &&
  timingSafeEqual(Buffer.from(a), Buffer.from(b));
export const sleep = (ms: number, signal?: AbortSignal) =>
  new Promise<void>((resolve, reject) => {
    if (signal?.aborted) return reject(signal.reason);
    const abort = () => {
      clearTimeout(timer);
      reject(signal?.reason);
    };
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", abort);
      resolve();
    }, ms);
    signal?.addEventListener("abort", abort, { once: true });
  });
export async function atomic(file: string, value: unknown) {
  await mkdir(dirname(file), { recursive: true, mode: 0o700 });
  const temp = `${file}.${id()}.tmp`;
  const handle = await open(temp, "wx", 0o600);
  try {
    await handle.writeFile(
      typeof value === "string" ? value : JSON.stringify(value, null, 2) + "\n",
    );
    await handle.sync();
  } catch (error) {
    await unlink(temp).catch(() => {});
    throw error;
  } finally {
    await handle.close();
  }
  try {
    await rename(temp, file);
  } catch (error) {
    await unlink(temp).catch(() => {});
    throw error;
  }
  const directory = await open(dirname(file), "r");
  try {
    await directory.sync();
  } finally {
    await directory.close();
  }
}
/** Private Crow state reader. T describes trusted persisted data, not validation of external JSON. */
export async function json<T = unknown>(
  file: string,
  fallback: T | null = null,
): Promise<T | null> {
  try {
    return JSON.parse(await readFile(file, "utf8")) as T;
  } catch (e) {
    if (errorCode(e) === "ENOENT") return fallback;
    throw e;
  }
}
export function cleanEnv(extra: NodeJS.ProcessEnv = {}): NodeJS.ProcessEnv {
  const env = Object.fromEntries(
    [
      "PATH",
      "HOME",
      "USER",
      "LANG",
      "LC_ALL",
      "TMPDIR",
      "SSL_CERT_FILE",
      "CODEX_CA_CERTIFICATE",
    ]
      .filter((k) => process.env[k])
      .map((k) => [k, process.env[k]]),
  );
  return {
    ...env,
    GIT_TERMINAL_PROMPT: "0",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: "/dev/null",
    NO_COLOR: "1",
    ...extra,
  };
}
export function hostEnv(extra: NodeJS.ProcessEnv = {}): NodeJS.ProcessEnv {
  const session = Object.fromEntries(
    ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"]
      .filter((key) => process.env[key])
      .map((key) => [key, process.env[key]]),
  );
  return { ...cleanEnv(), ...session, ...extra };
}
export function processRun(
  command: string,
  args: string[],
  {
    cwd,
    env = hostEnv(),
    input,
    signal,
    timeout = 0,
    limit = 16 * 1024 * 1024,
    onLine,
    onChunk,
    inherit = false,
    detached = !inherit,
    capture = true,
  }: ProcessOptions = {},
): Promise<ProcessResult> {
  return new Promise<ProcessResult>((resolve, reject) => {
    if (signal?.aborted)
      return reject(signal.reason ?? new Error("Interrupted"));
    const child = spawn(command, args, {
      cwd,
      env,
      detached,
      stdio: inherit ? "inherit" : ["pipe", "pipe", "pipe"],
    });
    let out = "",
      err = "",
      pending = "",
      bytes = 0,
      failure: unknown,
      killer: NodeJS.Timeout | undefined,
      timer: NodeJS.Timeout | undefined;
    const stop = (reason: unknown) => {
      failure ??= reason;
      if (!child.pid) return;
      try {
        if (detached) process.kill(-child.pid, "SIGINT");
        else child.kill("SIGINT");
      } catch {}
      killer ??= setTimeout(() => {
        try {
          if (detached && child.pid) process.kill(-child.pid, "SIGKILL");
          else child.kill("SIGKILL");
        } catch {}
      }, 2000);
    };
    const abort = () => stop(signal?.reason ?? new Error("Interrupted"));
    signal?.addEventListener("abort", abort, { once: true });
    if (timeout)
      timer = setTimeout(
        () => stop(new Error(`${command} timed out`)),
        timeout,
      );
    if (!inherit) {
      child.stdout!.setEncoding("utf8");
      child.stderr!.setEncoding("utf8");
      child.stdout!.on("data", (data: string) => {
        if (failure) return;
        if (onChunk) {
          try {
            onChunk(data);
          } catch (e) {
            stop(e);
            return;
          }
        }
        if (capture) {
          bytes += Buffer.byteLength(data);
          out += data;
        }
        if (onLine) {
          pending += data;
          const lines = pending.split("\n");
          pending = lines.pop() ?? "";
          if (
            Buffer.byteLength(pending) > limit ||
            lines.some((line) => Buffer.byteLength(line) > limit)
          ) {
            stop(new Error("Process output line limit exceeded"));
            return;
          }
          for (const line of lines) {
            try {
              onLine(line);
            } catch (e) {
              stop(e);
            }
          }
        }
        if (bytes > limit) stop(new Error("Process output limit exceeded"));
      });
      child.stderr!.on("data", (data: string) => {
        if (failure) return;
        if (capture) bytes += Buffer.byteLength(data);
        err = (err + data).slice(-65536);
        if (bytes > limit) stop(new Error("Process output limit exceeded"));
      });
      child.stdin!.on("error", () => {});
      child.stdin!.end(input ?? "");
    }
    child.on("error", (e) => {
      failure = e;
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      clearTimeout(killer);
      signal?.removeEventListener("abort", abort);
      // The captured process group belongs to this invocation, including MCP helpers.
      try {
        if (child.pid && detached) process.kill(-child.pid, "SIGKILL");
      } catch {}
      if (failure) reject(failure);
      else if (code !== 0)
        reject(
          Object.assign(
            new Error(`${command} exited ${code}: ${err.slice(-2000)}`),
            { stderr: err, stdout: out, code },
          ),
        );
      else resolve({ stdout: out, stderr: err });
    });
  });
}
export function repoName(value: string) {
  if (
    !/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(value) ||
    value.split("/").some((x) => [".", ".."].includes(x))
  )
    throw new Error("Expected owner/repository");
  return value.toLowerCase();
}
export function httpsUrl(value: string) {
  const u = new URL(value);
  if (
    u.protocol !== "https:" ||
    u.username ||
    u.password ||
    u.search ||
    u.hash ||
    u.pathname !== "/"
  )
    throw new Error("Expected an HTTPS origin without a path or credentials");
  return u.origin;
}
export function integer(
  value: unknown,
  min: number,
  max: number,
  name: string,
): number {
  if (
    typeof value !== "number" ||
    !Number.isInteger(value) ||
    value < min ||
    value > max
  )
    throw new Error(`${name} must be an integer between ${min} and ${max}`);
  return value;
}
export async function acquireLock(file: string) {
  const { open, unlink } = await import("node:fs/promises");
  await mkdir(dirname(file), { recursive: true, mode: 0o700 });
  const start = (pid: number) =>
    readFile(`/proc/${pid}/stat`, "utf8")
      .then((s) => s.slice(s.lastIndexOf(")") + 2).split(" ")[19])
      .catch(() => null);
  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      const handle = await open(file, "wx", 0o600);
      const value = {
        pid: process.pid,
        start: await start(process.pid),
        nonce: id(),
      };
      await handle.writeFile(JSON.stringify(value));
      await handle.close();
      return async () => {
        if ((await json<{ nonce: string }>(file, null))?.nonce === value.nonce)
          await unlink(file);
      };
    } catch (e) {
      if (errorCode(e) !== "EEXIST") throw e;
      const old = await json<{ pid: number; start: string | null }>(file, null);
      if (!old?.pid || old.start === (await start(old.pid)))
        throw new Error(
          "Crow is running, or its runtime lock needs inspection. Stop Crow before continuing.",
        );
      await unlink(file);
    }
  }
  throw new Error("Could not acquire Crow runtime lock");
}
