/** Domain contracts shared by Crow's service, worker and provider adapters. */
export type Role = "both" | "service" | "worker";
export interface RetrySettings {
  mode: "fixed" | "progressive";
  count: number;
  delayMs: number;
}
export type SubagentSettings =
  | { mode: "inherit"; max: number; model?: string; effort?: string }
  | { mode: "configured"; max: number; model: string; effort: string };
export interface ReviewSettings {
  model: string | null;
  effort: string | null;
  subagents: SubagentSettings;
  retry: RetrySettings;
  timeoutMs: number;
}
export interface CodexProxy {
  baseUrl: string;
  configFile: string;
}
export interface RuntimeReviewSettings extends ReviewSettings {
  codex: string;
  codexHome: string;
  codexProxy?: CodexProxy | null;
  detached?: boolean;
}
export interface WorkerConfig extends RuntimeReviewSettings {
  id: string;
  token: string;
  concurrency: number;
}
export interface RepositorySettings {
  model?: string | null;
  effort?: string | null;
  timeoutMs?: number;
  subagents?: {
    mode?: "inherit" | "configured";
    max?: number;
    model?: string;
    effort?: string;
  };
  retry?: Partial<RetrySettings>;
}
export interface GitHubApp {
  id: number | string;
  pem: string;
  webhookSecret: string;
  slug: string;
  botId?: number | null;
}
export interface Ingress {
  type: "funnel" | "cloudflare" | "existing";
  port?: number;
  target?: string;
  pending?: boolean;
  file?: string;
  tunnel?: string;
}
export interface AppRegistration {
  ownerType: "personal" | "organization";
  organization?: string;
  visibility: "public" | "private";
}
export interface CrowConfig {
  version: 1;
  role: Role;
  operator: string | null;
  publicUrl: string | null;
  port: number;
  bind: string;
  adminToken: string;
  serviceUrl: string;
  worker: WorkerConfig;
  catchUp: { enabled: boolean; threshold: number };
  auditIntervalMs: number;
  retentionDays: number;
  ingress: Ingress;
  app: GitHubApp | null;
  appRegistration?: AppRegistration;
}
export interface Comparison {
  head: string;
  base: string;
  target: string;
  targetSha: string;
}
export interface InspectionSource extends Comparison {
  dir: string;
}
export interface GuidanceFile {
  path: string;
  body: string;
}
export interface Guidance {
  files: GuidanceFile[];
  fingerprint: string;
  targetSha?: string;
}
export type Severity = "critical" | "high" | "medium" | "low";
export interface FindingInput {
  title: string;
  body: string;
  path: string;
  line: number;
  severity: Severity;
}
export interface Finding extends FindingInput {
  id: string;
}
export interface ReviewReport {
  summary: string;
  findings: Finding[];
}
export type ReviewState =
  | "queued"
  | "held"
  | "reviewing"
  | "retrying"
  | "paused"
  | "publishing"
  | "completed"
  | "superseded"
  | "cancelled";
export interface ReviewJob {
  id: string;
  key: string;
  repo: string;
  number: number;
  head: string;
  target: string;
  worker: string;
  state: ReviewState;
  manual: boolean;
  restart: boolean;
  trigger?: string;
  priority: number;
  resumeEpoch: number;
  createdAt: number;
  updatedAt: number;
  retries: number;
  nextAt: number;
  session?: string | null;
  report?: ReviewReport | null;
  settings?: ReviewSettings;
  comparison?: Comparison;
  patch?: string;
  lease?: string;
  startedAt?: number;
  author?: string;
  reason?: string | null;
  autoRecover?: boolean;
  publishAt?: number;
  reviewUrl?: string;
  guidanceFingerprint?: string;
  guidanceTargetSha?: string;
  settingsChanges?: {
    model: string | null;
    effort: string | null;
    at: number;
  }[];
  prContext?: { title: string; body: string };
  parentId?: string;
  taskId?: string;
}
export type PreparedReviewJob = Omit<ReviewJob, "settings"> & {
  settings: RuntimeReviewSettings;
  comparison: Comparison;
};
export type PublishableReviewJob = ReviewJob & {
  settings: ReviewSettings;
  comparison: Comparison;
  report: ReviewReport;
};
export interface RepositoryRecord {
  name: string;
  installation: number;
  worker: string;
  policy: "selected" | "everyone";
  authors: string[];
  requesters: string[];
  settings: RepositorySettings;
  enrolledAt: number;
  excluded: number[];
}
export interface WorkerRecord {
  id: string;
  token: string;
  lastSeen?: number;
  defaults?: ReviewSettings & { concurrency: number };
}
export interface FindingHistory {
  id: string;
  title: string;
  url: string;
}
export interface CompletedComparison {
  job?: string;
  reviewUrl: string;
  comparison?: Comparison;
}
export interface StatusRecord {
  id: number;
  body: string;
  state: ReviewState;
  trigger?: string;
  updatedAt: number;
}
export interface RecordMap {
  jobs: ReviewJob;
  repos: RepositoryRecord;
  workers: WorkerRecord;
  findings: FindingHistory[];
  completed: CompletedComparison;
  status: StatusRecord;
  state: boolean | number;
}
export interface CrowEvent {
  type: string;
  repo?: string;
  number?: number;
  action?: string;
  occurredAt?: number;
  request?: boolean;
  command?: "review" | "resume" | "restart" | "pause";
  actor?: string;
  ref?: string;
}
export interface GitHubUser {
  id: number;
  login: string;
}
export interface GitHubPullRequest {
  number: number;
  state: string;
  draft: boolean;
  user: Pick<GitHubUser, "login">;
  head: { sha: string };
  base: { sha: string; ref: string };
  title?: string;
  body?: string | null;
  updated_at?: string;
}
export interface GitHubComment {
  id: number;
  body: string;
  user: Pick<GitHubUser, "id"> | null;
  html_url?: string;
  url?: string;
}
export interface GitHubReview extends GitHubComment {
  html_url: string;
  commit_id?: string;
}
export interface InlineComment {
  path: string;
  line: number;
  side: "RIGHT";
  body: string;
}
export interface ReviewMetadata {
  job?: string;
  head?: string;
  base?: string;
  target?: string;
  guidance?: string;
  model?: string | null;
  effort?: string | null;
}
export interface ProviderModel {
  id?: string;
  model: string;
  defaultReasoningEffort: string;
  supportedReasoningEfforts: {
    reasoningEffort: string;
    description?: string;
  }[];
  isDefault?: boolean;
  displayName?: string;
  description?: string;
}
export interface ProviderCatalog {
  models: ProviderModel[];
  account: { type: string; email?: string; planType?: string };
  retrievedAt?: string;
  cached: boolean;
  warning: string | null;
}
