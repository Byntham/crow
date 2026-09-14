#!/usr/bin/env node
import { readFile, rm } from "node:fs/promises";
import { join } from "node:path";
import { isMain, version } from "../lib/runtime.mjs";
import { home, load, save, settings, validateConfig } from "../lib/config.mjs";
import {
  admin,
  doctor,
  serviceAction,
  update,
  unitName,
  updateAvailability,
} from "../lib/operations.mjs";
import { processRun, acquireLock, atomic, id } from "../lib/util.mjs";
import type { CrowConfig, ReviewSettings } from "../lib/types.mjs";

type Flags = Record<string, string | boolean | undefined>;
interface PolicyChanges {
  policy?: "everyone" | "selected";
  authors?: string[];
  requesters?: string[];
}
function textFlag(flags: Flags, name: string): string | undefined {
  const value = flags[name];
  if (typeof value === "boolean")
    throw new Error(`Provide a value for --${name}`);
  return value;
}
function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
const help = `Crow: self-hosted GitHub PR reviews\n\n  crow install [--no-setup]         Install a downloaded binary\n  crow setup [--role both|service|worker] [--ingress funnel|cloudflare|existing] [--port 8787]\n  crow run                         Run in the foreground\n  crow start | stop | service-restart\n  crow status | doctor [--runtime] | logs\n  crow login | models\n  crow enroll owner/repo [--include-backlog] [--reenroll] [--worker ID]\n  crow policy owner/repo [--authors alice,bob | --everyone] [--requesters alice,bob]\n  crow repo-config owner/repo --json '{"model":"...","effort":"..."}'\n  crow review|pause|resume|restart owner/repo PR_NUMBER [--model ID --effort LEVEL]\n  crow catch-up [owner/repo] [--include-backlog]\n  crow release [owner/repo]          Release a held catch-up batch\n  crow pair                        Generate credentials for a separate worker\n  crow config [KEY JSON_VALUE]      View redacted config or set a setting\n  crow cleanup | update\n  crow drain | undrain             Hold or resume new review claims\n  crow backup FILE --passphrase-file FILE\n  crow restore FILE --passphrase-file FILE\n\nCROW_HOME chooses the installation directory. Default ~/.local/share/crow.\nBrowser URLs printed on a headless host can be opened on another desktop.\n`;
export function parse(argv: string[]) {
  const args: string[] = [],
    flags: Flags = {};
  for (let i = 0; i < argv.length; i++) {
    const s = argv[i];
    if (s.startsWith("--")) {
      const [key, inline] = s.slice(2).split(/=(.*)/s);
      if (inline !== undefined) flags[key] = inline;
      else if (
        ["include-backlog", "everyone", "runtime", "no-setup"].includes(key)
      )
        flags[key] = true;
      else if (argv[i + 1] && !argv[i + 1].startsWith("--"))
        flags[key] = argv[++i];
      else flags[key] = true;
    } else args.push(s);
  }
  return { args, flags };
}
export function policyChanges(flags: Flags): PolicyChanges {
  const changes: PolicyChanges = {};
  const logins = (value: unknown): string[] => {
    if (typeof value !== "string")
      throw new Error("Provide a comma-separated GitHub login list");
    const list = value.split(",").map((login) => login.trim());
    if (
      !list.length ||
      list.some(
        (login) =>
          !/^[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?(?:\[bot\])?$/.test(
            login,
          ),
      )
    )
      throw new Error(
        "Expected comma-separated GitHub logins without empty entries",
      );
    return [...new Set(list.map((login) => login.toLowerCase()))];
  };
  if (flags.everyone && flags.authors !== undefined)
    throw new Error("Choose --authors or --everyone, not both");
  if (flags.everyone) changes.policy = "everyone";
  if (flags.authors !== undefined) {
    changes.policy = "selected";
    changes.authors = logins(flags.authors);
  }
  if (flags.requesters !== undefined)
    changes.requesters = logins(flags.requesters);
  if (!Object.keys(changes).length)
    throw new Error("Specify --authors, --everyone, or --requesters");
  return changes;
}
async function validateModels(
  config: CrowConfig,
  root: string,
  worker: ReviewSettings,
) {
  if (config.role === "service") return;
  const catalog = await (
    await import("../lib/provider.mjs")
  ).discover(config.worker, root);
  if (catalog.warning) console.error(catalog.warning);
  const check = (
    model: string | null | undefined,
    effort: string | null | undefined,
  ) => {
    const item = catalog.models.find(
      (m) => m.id === model || m.model === model,
    );
    if (!item)
      throw new Error(`Model ${model} is absent from the provider catalog`);
    if (
      !item.supportedReasoningEfforts.some((e) => e.reasoningEffort === effort)
    )
      throw new Error(`Unsupported reasoning level ${effort} for ${model}`);
  };
  check(worker.model, worker.effort);
  if (worker.subagents.mode === "configured")
    check(worker.subagents.model, worker.subagents.effort);
}
export function redact(config: CrowConfig) {
  const c = structuredClone(config);
  c.adminToken = "[hidden]";
  c.worker.token = "[hidden]";
  if (c.app) {
    c.app.pem = "[hidden]";
    c.app.webhookSecret = "[hidden]";
  }
  return c;
}
async function runCrow(config: CrowConfig, root: string) {
  const release = await acquireLock(join(root, "runtime.lock"));
  let service: { close(): Promise<void> } | undefined;
  let worker: { close(): Promise<void>; drain(): Promise<void> } | undefined;
  let closing = false;
  const close = async () => {
    if (closing) return;
    closing = true;
    await rm(join(root, "ready.json"), { force: true });
    await worker?.close();
    await service?.close();
    await release();
  };
  try {
    if (config.role !== "worker") {
      const { startService } = await import("../lib/service.mjs");
      service = await startService(config, root);
    }
    if (config.role !== "service") {
      const { startWorker } = await import("../lib/worker.mjs");
      worker = await startWorker(config, root);
    }
    for (const signal of ["SIGINT", "SIGTERM"])
      process.once(signal, () =>
        close().then(
          () => process.exit(0),
          (e) => {
            console.error(errorMessage(e));
            process.exit(1);
          },
        ),
      );
    process.on("SIGUSR1", async () => {
      try {
        await worker?.drain?.();
        await atomic(join(root, "drained.json"), {
          pid: process.pid,
          at: Date.now(),
        });
      } catch (e) {
        console.error(`Drain failed: ${errorMessage(e)}`);
      }
    });
    await atomic(join(root, "ready.json"), {
      pid: process.pid,
      version: version(),
    });
    console.log(`Crow ${config.role} running.`);
    await new Promise(() => {});
  } catch (e) {
    await close();
    throw e;
  }
}
export async function main(argv = process.argv.slice(2)) {
  if (argv[0] === "_inspection-mcp") {
    if (!argv[1]) throw new Error("Inspection source is required");
    return (await import("./inspection-mcp.mjs")).inspectionMain(
      argv[1],
      argv[2],
    );
  }
  const { args, flags } = parse(argv),
    [command = "help", ...rest] = args,
    root = home();
  const print = (x: unknown) =>
    console.log(typeof x === "string" ? x : JSON.stringify(x, null, 2));
  if (command === "version" || flags.version) {
    print(version());
    return;
  }
  if (["help", "-h", "--help"].includes(command) || flags.help) {
    print(help);
    return;
  }
  if (command === "setup") {
    await (
      await import("../lib/setup.mjs")
    ).setup(root, {
      role: textFlag(flags, "role"),
      ingress: textFlag(flags, "ingress"),
      port: flags.port,
    });
    return;
  }
  if (command === "install") {
    if (
      rest.length ||
      Object.keys(flags).some((key) => key !== "no-setup") ||
      (flags["no-setup"] !== undefined && flags["no-setup"] !== true)
    )
      throw new Error("Usage: crow install [--no-setup]");
    return (await import("../lib/install-command.mjs")).installCommand(
      root,
      flags["no-setup"] === true,
    );
  }
  if (command === "restore" || command === "backup") {
    if (!rest[0] || typeof flags["passphrase-file"] !== "string")
      throw new Error(
        `Usage: crow ${command} ARCHIVE --passphrase-file SECRET_FILE`,
      );
    const secret = (await readFile(flags["passphrase-file"], "utf8")).replace(
      /\r?\n$/,
      "",
    );
    const backup = await import("../lib/backup.mjs");
    print(
      await (command === "backup" ? backup.exportBackup : backup.restoreBackup)(
        root,
        rest[0],
        secret,
      ),
    );
    return;
  }
  const config = await load(root);
  if (command === "run") return runCrow(config, root);
  if (["start", "stop", "service-restart"].includes(command))
    return serviceAction(
      root,
      command === "service-restart" ? "restart" : command,
    );
  if (command === "logs")
    return processRun("journalctl", ["--user", "-u", unitName(root), "-f"], {
      inherit: true,
    });
  if (command === "doctor") {
    const result = await doctor(config, root, { runtime: !!flags.runtime });
    print(result);
    if (!result.ok) process.exitCode = 1;
    return;
  }
  if (command === "login") {
    if (config.role === "service") throw new Error("This host has no worker.");
    await (await import("../lib/provider.mjs")).login(config.worker, root);
    return;
  }
  if (command === "models") {
    const result = await (
      await import("../lib/provider.mjs")
    ).discover(config.worker, root);
    if (result.warning) console.error(result.warning);
    print(result);
    return;
  }
  if (command === "config") {
    if (!rest.length) {
      print(redact(config));
      return;
    }
    const writable = [
      "worker.concurrency",
      "worker.model",
      "worker.effort",
      "worker.subagents",
      "worker.retry",
      "worker.timeoutMs",
      "catchUp.enabled",
      "catchUp.threshold",
      "auditIntervalMs",
      "retentionDays",
    ];
    if (!writable.includes(rest[0]) || rest[1] === undefined)
      throw new Error(
        `Set one of: ${writable.join(", ")}. Values use JSON syntax.`,
      );
    const candidate: unknown = JSON.parse(JSON.stringify(config));
    if (!isRecord(candidate)) throw new Error("Invalid configuration");
    let object = candidate;
    const parts = rest[0].split(".");
    for (const key of parts.slice(0, -1)) {
      const child = object[key];
      if (!isRecord(child))
        throw new Error(`Invalid configuration section ${key}`);
      object = child;
    }
    const key = parts.at(-1);
    if (!key) throw new Error("Missing configuration key");
    object[key] = JSON.parse(rest[1]) as unknown;
    const updated = validateConfig(candidate);
    if (["worker.model", "worker.effort", "worker.subagents"].includes(rest[0]))
      await validateModels(updated, root, updated.worker);
    await save(updated, root);
    print("Configuration saved. Run crow service-restart to apply it.");
    return;
  }
  if (command === "update") {
    print(await update(root, config));
    return;
  }
  if (["drain", "undrain"].includes(command)) {
    if (rest.length || Object.keys(flags).length)
      throw new Error(`Usage: crow ${command}`);
    print(await admin(config, command, {}));
    return;
  }
  if (command === "status") {
    if (config.role === "worker") {
      const { json } = await import("../lib/util.mjs");
      print(
        await json(join(root, "worker-status.json"), {
          message: "Run crow doctor and crow logs for worker status.",
        }),
      );
    } else print(await admin(config, "status"));
    const updates = await updateAvailability(root);
    if (updates.available)
      print("A Crow update is available. Run crow update when ready.");
    if (updates.warning) console.error(updates.warning);

    return;
  }
  if (command === "enroll") {
    if (!rest[0]) throw new Error("Usage: crow enroll owner/repo");
    if (config.role === "service" && !flags.worker)
      throw new Error(
        "Choose a paired worker with --worker ID. Run crow pair first.",
      );
    const { githubIdentity } = await import("../lib/setup.mjs");
    const identity = await githubIdentity();
    print(
      await admin(config, "enroll", {
        repo: rest[0],
        githubToken: identity.token,
        worker: flags.worker || config.worker.id,
        policy: "selected",
        authors: [config.operator],
        includeBacklog: !!flags["include-backlog"],
        reenroll: !!flags.reenroll,
      }),
    );
    return;
  }
  if (command === "pair") {
    const worker = { id: id(), token: id() + id() };
    print(await admin(config, "pair", worker));
    print({ serviceUrl: config.publicUrl, ...worker });
    return;
  }
  if (command === "policy") {
    if (!rest[0])
      throw new Error(
        "Usage: crow policy owner/repo [--authors alice,bob | --everyone] [--requesters alice,bob]",
      );
    print(
      await admin(config, "config-repo", {
        repo: rest[0],
        ...policyChanges(flags),
      }),
    );
    return;
  }
  if (command === "repo-config") {
    if (!rest[0] || typeof flags.json !== "string")
      throw new Error(
        "Usage: crow repo-config owner/repo --json SETTINGS_JSON",
      );
    const override: unknown = JSON.parse(flags.json);
    await validateModels(
      config,
      root,
      settings(config, { settings: override }),
    );
    print(
      await admin(config, "config-repo", { repo: rest[0], settings: override }),
    );
    return;
  }
  if (["review", "pause", "resume", "restart"].includes(command)) {
    const number = Number(rest[1]);
    if (!rest[0] || !Number.isSafeInteger(number) || number < 1)
      throw new Error(`Usage: crow ${command} owner/repo PR_NUMBER`);
    if ((flags.model || flags.effort) && command !== "resume")
      throw new Error(
        "Model/effort overrides apply to resume only. Configure defaults for new reviews.",
      );
    print(
      await admin(config, command, {
        repo: rest[0],
        number,
        ...(flags.model ? { model: flags.model } : {}),
        ...(flags.effort ? { effort: flags.effort } : {}),
      }),
    );
    return;
  }
  if (command === "cleanup") {
    if (config.role !== "worker") print(await admin(config, "cleanup", {}));
    if (config.role !== "service") {
      const { workerRequest } = await import("../lib/worker.mjs");
      const { cleanup } = await import("../lib/retention.mjs");
      const state = await workerRequest(config, "maintenance");
      print(await cleanup(root, state.jobs, state.retentionDays));
    }
    return;
  }
  if (["catch-up", "release"].includes(command)) {
    print(
      await admin(config, command, {
        ...(rest[0] ? { repo: rest[0] } : {}),
        includeBacklog: !!flags["include-backlog"],
      }),
    );
    return;
  }
  throw new Error(`Unknown command ${command}. Run crow help.`);
}
if (isMain(import.meta.url))
  main().catch((e) => {
    console.error(`Crow: ${errorMessage(e)}`);
    process.exitCode = 1;
  });
