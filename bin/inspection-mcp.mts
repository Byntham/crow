#!/usr/bin/env node
import { createInterface } from "node:readline";
import { readFile } from "node:fs/promises";
import { inspectionTool } from "../lib/inspection.mjs";
import { Delegation, delegationTools } from "../lib/delegation.mjs";
import type { InspectionSource } from "../lib/types.mjs";
import type { RunReviewOptions } from "../lib/provider.mjs";
import { isRecord, errorMessage } from "../lib/util.mjs";
import { isMain } from "../lib/runtime.mjs";
export async function inspectionMain(sourcePath: string, contextPath?: string) {
  // These private files are written by Crow, rather than supplied by the reviewer.
  const source = JSON.parse(
    await readFile(sourcePath, "utf8"),
  ) as InspectionSource;
  const context: RunReviewOptions | null = contextPath
    ? (JSON.parse(await readFile(contextPath, "utf8")) as RunReviewOptions)
    : null;
  const delegation =
    context && context.job.settings.subagents.max > 0
      ? await new Delegation(context).init()
      : null;
  interface ToolDefinition {
    name: string;
    description: string;
    properties: Record<string, unknown>;
    required?: string[];
  }
  const definitions: ToolDefinition[] = [
    {
      name: "list_files",
      description:
        "List files with pagination metadata. Set changed_only=true to enumerate every changed path against the merge base, including deleted files; otherwise list all tracked files at the PR commit. Follow nextOffset until null before treating the list as complete.",
      properties: {
        prefix: { type: "string" },
        changed_only: { type: "boolean" },
        offset: { type: "integer", minimum: 0 },
        count: { type: "integer", minimum: 1, maximum: 10000 },
      },
    },
    {
      name: "read_file",
      description:
        "Read numbered lines of a regular tracked file. Symlinks are never followed.",
      properties: {
        path: { type: "string" },
        revision: { enum: ["head", "base"] },
        start: { type: "integer" },
        count: { type: "integer" },
      },
      required: ["path"],
    },
    {
      name: "diff",
      description:
        "Read a page of the PR diff against its merge base, optionally for one path. Returns patch plus pagination metadata. offset/count use character offsets, not lines. Follow nextOffset until null to read the complete diff; a truncated page is not the entire comparison. Use list_files with changed_only=true to discover paths independently of diff size.",
      properties: {
        path: { type: "string" },
        offset: { type: "integer", minimum: 0 },
        count: { type: "integer", minimum: 1, maximum: 200000 },
      },
    },
    {
      name: "search",
      description:
        "Search tracked source using a literal string. Does not execute repository code.",
      properties: { text: { type: "string" }, path: { type: "string" } },
      required: ["text"],
    },
    ...(delegation ? delegationTools : []),
  ];
  const tools = definitions.map(({ properties, required = [], ...x }) => ({
    ...x,
    inputSchema: {
      type: "object",
      properties,
      required,
      additionalProperties: false,
    },
  }));
  const send = (x: unknown) => process.stdout.write(JSON.stringify(x) + "\n");
  let closing = false,
    closePromise: Promise<void> | undefined;

  const stop = () => {
    if (closing) return;
    closing = true;
    closePromise = delegation?.close();
    process.stdin.destroy();
  };
  process.on("SIGINT", stop);
  process.on("SIGTERM", stop);
  try {
    for await (const line of createInterface({ input: process.stdin })) {
      let r: Record<string, unknown> | undefined;
      try {
        const parsed: unknown = JSON.parse(line);
        if (!isRecord(parsed)) throw new Error("Invalid request");
        r = parsed;
        if (r.id === undefined) continue;
        let result;
        if (r.method === "initialize")
          result = {
            protocolVersion: "2024-11-05",
            capabilities: { tools: {} },
            serverInfo: { name: "crow-inspection", version: "1.0.0" },
          };
        else if (r.method === "tools/list") result = { tools };
        else if (r.method === "ping") result = {};
        else if (r.method === "tools/call") {
          try {
            if (!isRecord(r.params) || typeof r.params.name !== "string")
              throw new Error("Invalid tool request");
            const { name, arguments: args } = r.params;
            const value =
              delegationTools.some((t) => t.name === name) && delegation
                ? await delegation.call(name, args || {})
                : await inspectionTool(source, name, args || {});
            result = {
              content: [
                {
                  type: "text",
                  text:
                    typeof value === "string" ? value : JSON.stringify(value),
                },
              ],
            };
          } catch (e) {
            result = {
              isError: true,
              content: [{ type: "text", text: errorMessage(e) }],
            };
          }
        } else {
          send({
            jsonrpc: "2.0",
            id: r.id,
            error: { code: -32601, message: "Unsupported method" },
          });
          continue;
        }
        send({ jsonrpc: "2.0", id: r.id, result });
      } catch {
        if (r?.id !== undefined)
          send({
            jsonrpc: "2.0",
            id: r.id,
            error: { code: -32600, message: "Invalid request" },
          });
      }
    }
  } finally {
    await (closePromise || delegation?.close());
    process.removeListener("SIGINT", stop);
    process.removeListener("SIGTERM", stop);
  }
}
if (isMain(import.meta.url))
  inspectionMain(process.argv[2], process.argv[3]).catch((error: unknown) => {
    console.error(errorMessage(error));
    process.exitCode = 1;
  });
