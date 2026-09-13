import type {
  FindingInput,
  Severity,
  ReviewReport,
  PublishableReviewJob,
  FindingHistory,
  InlineComment,
  ReviewMetadata,
} from "./types.mjs";
import { hash } from "./util.mjs";
import { safePath } from "./inspection.mjs";
export const schema = {
  type: "object",
  additionalProperties: false,
  properties: {
    summary: { type: "string" },
    findings: {
      type: "array",
      items: {
        type: "object",
        additionalProperties: false,
        properties: {
          title: { type: "string" },
          body: { type: "string" },
          path: { type: "string" },
          line: { type: "integer" },
          severity: { enum: ["critical", "high", "medium", "low"] },
        },
        required: ["title", "body", "path", "line", "severity"],
      },
    },
  },
  required: ["summary", "findings"],
};
export function validateReport(input: unknown): ReviewReport {
  const value = record(input);
  if (
    !value ||
    typeof value.summary !== "string" ||
    !value.summary.trim() ||
    value.summary.length > 16000 ||
    !Array.isArray(value.findings) ||
    value.findings.length > 100
  )
    throw new Error("Invalid final review report");
  const findings: FindingInput[] = [];
  for (const entry of value.findings) {
    const f = record(entry);
    if (
      !f ||
      !severity(f.severity) ||
      typeof f.title !== "string" ||
      !f.title.trim() ||
      f.title.length > 300 ||
      typeof f.body !== "string" ||
      !f.body.trim() ||
      f.body.length > 4000 ||
      typeof f.line !== "number" ||
      !Number.isInteger(f.line) ||
      f.line < 1 ||
      typeof f.path !== "string" ||
      !safePath(f.path)
    )
      throw new Error("Invalid finding in final review report");
    findings.push({
      ...f,
      title: f.title,
      body: f.body,
      path: f.path,
      line: f.line,
      severity: f.severity,
    });
  }
  const report = {
    summary: value.summary,
    findings: findings.map((f) => ({
      ...f,
      id: hash([f.path, f.title.trim().toLowerCase()]).slice(0, 20),
    })),
  };
  if (
    Buffer.byteLength(JSON.stringify(report)) > 45000 ||
    report.summary.length + renderFindings(report).length > 50000
  )
    throw Object.assign(
      new Error(
        "Invalid final review report: shorten the completed report to fit GitHub publication limits",
      ),
      { kind: "output" },
    );
  return report;
}
export const marker = (meta: ReviewMetadata) =>
  `<!-- crow-review:v1 ${Buffer.from(JSON.stringify(meta)).toString("base64url")} -->`;
export function metadata(body: string): ReviewMetadata | null {
  try {
    const m = body.match(/<!-- crow-review:v1 ([A-Za-z0-9_-]+) -->/);
    if (!m) return null;
    const value = record(
      JSON.parse(Buffer.from(m[1], "base64url").toString("utf8")),
    );
    if (!value) return null;
    for (const key of ["job", "head", "base", "target", "guidance"])
      if (value[key] !== undefined && typeof value[key] !== "string")
        return null;
    for (const key of ["model", "effort"])
      if (value[key] != null && typeof value[key] !== "string") return null;
    return value as ReviewMetadata;
  } catch {
    return null;
  }
}
function renderFindings(report: ReviewReport) {
  return report.findings.length
    ? "\n\nFindings:\n" +
        report.findings
          .map(
            (f) =>
              `\n- **${f.severity}: ${f.title}** (${f.path}:${f.line})\n\n  ${f.body.replaceAll("\n", "\n  ")}`,
          )
          .join("\n")
    : "";
}
export function reportBody(
  job: PublishableReviewJob,
  history: FindingHistory[] = [],
  earlierReviews: { html_url?: string; url?: string }[] = [],
) {
  const c = job.comparison,
    root = `https://github.com/${job.repo}`,
    meta = {
      job: job.id,
      head: c.head,
      base: c.base,
      target: c.target,
      guidance: job.guidanceFingerprint,
      model: job.settings.model,
      effort: job.settings.effort,
    };
  const lines = [
    marker(meta),
    "## Crow review",
    "",
    `Reviewed [${c.head.slice(0, 8)}](${root}/commit/${c.head}) against merge base [${c.base.slice(0, 8)}](${root}/commit/${c.base}) for \`${c.target.replaceAll("`", "")}\`.`,
    "",
    job.report.summary,
  ];
  if (!job.report.findings.length) lines.push("", "No actionable findings.");
  // Include every current finding even when GitHub cannot create its inline anchor.
  const body = lines.join("\n") + renderFindings(job.report);
  if (body.length > 59000)
    throw Object.assign(
      new Error(
        "Invalid final review report: rendered findings are too large for GitHub",
      ),
      { kind: "output" },
    );
  const current = new Set(job.report.findings.map((f) => f.id)),
    earlier = history.filter((f) => !current.has(f.id));
  const historyLines = [];
  if (earlier.length)
    historyLines.push(
      "",
      "Earlier findings, not reassessed:",
      ...earlier.map(
        (f) => `- [${f.title.replace(/[\[\]\r\n]/g, " ")}](${f.url})`,
      ),
    );
  if (earlierReviews.length)
    historyLines.push(
      "",
      "Earlier reviews, findings not reassessed:",
      ...earlierReviews.map(
        (review, index) =>
          `- [Earlier review ${index + 1}](${review.html_url || review.url})`,
      ),
    );
  if (!historyLines.length) return body;
  const detailed = "\n" + historyLines.join("\n");
  if (body.length + detailed.length <= 60000) return body + detailed;
  // Collapse titles to distinct report links before resorting to the complete PR history.
  const urls = [
    ...new Set(
      [
        ...earlier.map((f) => f.url),
        ...earlierReviews.map((r) => r.html_url || r.url),
      ].filter(Boolean),
    ),
  ];
  const compact =
    "\n\nEarlier reviews, findings not reassessed:\n" +
    urls
      .map((url, index) => `- [Earlier review ${index + 1}](${url})`)
      .join("\n");
  if (body.length + compact.length <= 60000) return body + compact;
  const previous = urls.at(-1),
    previousLink =
      previous && previous.length < 500
        ? `[previous review](${previous}) and `
        : "";
  return (
    body +
    `\n\nEarlier findings not reassessed. See the ${previousLink}[PR review history](${root}/pull/${job.number}).`
  );
}
function changedLines(patch: string) {
  const map = new Map<string, Set<number>>();
  let path = "",
    line = 0;
  for (const s of patch.split("\n")) {
    if (s.startsWith("+++ b/")) {
      path = s.slice(6);
      map.set(path, new Set());
    } else if (s.startsWith("@@ ")) {
      const m = s.match(/\+(\d+)/);
      if (m) line = Number(m[1]);
    } else if (s.startsWith("+") && !s.startsWith("+++")) {
      map.get(path)?.add(line++);
    } else if (!s.startsWith("-") && !s.startsWith("\\")) line++;
  }
  return map;
}
export function inlineComments(
  report: ReviewReport,
  patch: string,
  history: FindingHistory[] = [],
): InlineComment[] {
  const lines = changedLines(patch),
    old = new Set(history.map((f) => f.id));
  return report.findings
    .filter((f) => !old.has(f.id) && lines.get(f.path)?.has(f.line))
    .map((f) => ({
      path: f.path,
      line: f.line,
      side: "RIGHT",
      body: `**${f.severity}: ${f.title}**\n\n${f.body}`,
    }));
}

function record(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

function severity(value: unknown): value is Severity {
  return (
    typeof value === "string" &&
    ["critical", "high", "medium", "low"].includes(value)
  );
}
