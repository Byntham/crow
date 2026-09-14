// These cases run through tsc only. A weakened contract makes an expected error fail.
import { reportBody } from "../lib/report.mjs";
import { settings } from "../lib/config.mjs";
import type { Store } from "../lib/store.mjs";
import type {
  CrowConfig,
  PublishableReviewJob,
  ReviewJob,
  RepositorySettings,
  SubagentSettings,
} from "../lib/types.mjs";

declare const storedJob: ReviewJob;
declare const publishableJob: PublishableReviewJob;
declare const config: CrowConfig;
declare const store: Store;

reportBody(publishableJob);
// @ts-expect-error A persisted job may have no completed report or comparison.
reportBody(storedJob);

store.put("jobs", storedJob.id, storedJob);
// @ts-expect-error Repository rows cannot contain a review job.
store.put("repos", storedJob.repo, storedJob);

const overrides: RepositorySettings = { model: "provider-model" };
settings(config, { settings: overrides });
const invalidOverride: RepositorySettings = {
  // @ts-expect-error Repository overrides must not set worker credentials.
  token: "not-a-repository-setting",
};
void invalidOverride;

const inherited: SubagentSettings = { mode: "inherit", max: 8 };
const configured: SubagentSettings = {
  mode: "configured",
  max: 8,
  model: "provider-model",
  effort: "high",
};
// @ts-expect-error Configured delegation requires an explicit model and effort.
const incomplete: SubagentSettings = { mode: "configured", max: 8 };
void [inherited, configured, incomplete];
