# Hosted distribution

These files prepare public installation of Crow while the source repository stays private. Nothing in a pull request deploys the site or publishes binaries. R2 has been enabled in the operator's Cloudflare account; the bucket, Pages project, domains, credentials, and first publication still need setup.

| Resource                               | Name            | Public address                  |
| -------------------------------------- | --------------- | ------------------------------- |
| Cloudflare Pages direct-upload project | `crow-site`     | `https://birdapp.dev`           |
| R2 bucket                              | `crow-releases` | `https://downloads.birdapp.dev` |

Pages serves `website/`, including `install.sh`. R2 serves archives larger than Pages' 25 MiB file limit. Neither resource runs Crow or handles review traffic. Gibo is not part of the download service.

## Create Cloudflare resources

1. In Cloudflare R2, create a bucket named `crow-releases` using Standard storage. Keep the `r2.dev` development URL disabled. The bucket must contain only intended public release files.
2. In the bucket's **Settings → Custom domains**, connect `downloads.birdapp.dev`. Use this R2 flow instead of making a DNS CNAME manually. Cloudflare creates the DNS record and provisions TLS. Wait for the domain to report Active.
3. Leave downloads accessible without Cloudflare Access authentication, browser challenges, or bot checks. The installer and updater use ordinary HTTPS requests.
4. Do not add a cache rule that overrides the origin's `Cache-Control` for `/latest.txt`. Versioned release objects use one-year immutable caching; `latest.txt` uses `no-store, max-age=0`. If the zone already has a broad cache rule, add a bypass rule for hostname `downloads.birdapp.dev` and path `/latest.txt`.
5. After adding the Pages credentials below and making the workflows available, manually run **Prepare Crow Pages project** with confirmation `create crow-site`. It creates an empty project named `crow-site` with production branch `main` and no Git integration. If the matching project already exists, it verifies it without changing it. It uploads no site files. This avoids the dashboard drag-and-drop flow, which requires a deployment during creation.
6. In that Pages project's **Custom domains**, add `birdapp.dev`. Follow Cloudflare's DNS instructions and wait for TLS to become active. Check for an existing site or DNS record before replacing it.
7. Honor the site's `_headers` file. Exclude `birdapp.dev/install.sh` from any account or zone cache rule that forces caching. Keep the installer accessible without browser authentication or challenges.

Connecting the R2 custom domain makes files in this dedicated bucket public. Do not upload source archives, configuration, credentials, or debug logs. Disable directory listings if another service is later placed in front of R2; the native custom domain does not provide bucket listing.

## Set up deployment credentials

Create separate credentials for the website and binary uploads. Enter secret values directly in GitHub's repository settings, never in the repository, workflow inputs, terminal command arguments, or chat.

Add the following **repository secrets** under **GitHub → Settings → Secrets and variables → Actions**. These are the secrets used by the workflows; there is no need to copy them into environments.

The workflows identify `public-release` and `public-website` environments. If supported by the repository's GitHub plan, configure deployment branch restrictions and required reviewers there before first use. Manual workflow confirmation remains required without environment reviewers. An environment secret with the same name overrides a repository secret, so avoid stale duplicates.

Required secrets:

| Used by          | Repository secret           | Value and permissions                                                                                   |
| ---------------- | --------------------------- | ------------------------------------------------------------------------------------------------------- |
| Both             | `CLOUDFLARE_ACCOUNT_ID`     | The 32-character account ID shown in Cloudflare. This is an identifier, not a credential.               |
| `public-release` | `CROW_R2_ACCESS_KEY_ID`     | Access Key ID from a dedicated R2 API token.                                                            |
| `public-release` | `CROW_R2_SECRET_ACCESS_KEY` | Secret Access Key from that same R2 token.                                                              |
| `public-website` | `CROW_PAGES_API_TOKEN`      | Cloudflare API token with **Account → Cloudflare Pages → Edit**, restricted to this Cloudflare account. |

Create an account-owned R2 token from **R2 → Manage R2 API tokens** with **Object Read & Write** and **Apply to specific buckets only → crow-releases**. The publisher reads existing objects to refuse changed replacements, and writes release objects. It does not need bucket administration, DNS permissions, or access to other buckets. R2's preset may also permit deletion; the publisher never deletes an object.

The Pages token needs no zone or DNS edit permissions. Cloudflare's Pages permission applies at account scope; it is not a project-scoped token. Keep this credential separate from R2 credentials and rotate it if exposed. Resource creation and custom domain attachment happen through the account owner during initial setup.

## Build without publishing

`.github/workflows/release.yml` builds and tests Linux x64 and ARM64 on native Ubuntu runners. It runs for pull requests, matching version tags, and manual dispatch, and uploads private GitHub Actions artifacts. PR builds validate the proposed merge; publication requires a separate tag or manual build of the reviewed source commit. It never creates a GitHub release or uploads to Cloudflare.

The build artifacts are `crow-linux-x64` and `crow-linux-arm64`. Each contains its archive, `SHA256SUMS`, and `build-metadata.json` recording the source commit, archive hash, version, architecture, and the packaged executable's actual `--version` output. Artifacts expire after seven days. Publish within that window or build a new version. Rebuilding an existing version can change its archive bytes and must not overwrite an already published version.

Workflows with `workflow_dispatch` normally need to exist on the repository's default branch before GitHub exposes manual dispatch in the UI or API. Preparing these files on a draft PR does not make them runnable there by itself. Once the workflows exist on the default branch, select the intended reviewed branch when dispatching. No commit, merge, tag, build, or publication is implied by these instructions.

## Publish a reviewed binary build

1. Complete a build and record its run ID, exact 40-character commit SHA, and stable version from `package.json`.
2. Review the source and the successful checks for both architectures. The first version can remain `0.2.0` because the premature GitHub release was removed and no hosted release has been published. Once a version has public hosted objects, its bytes are immutable.
3. Manually run **Publish hosted Crow binaries**, selecting the reviewed workflow revision. Supply the run ID, source commit, version without `v`, and confirmation `publish VERSION`, for example `publish 0.2.0`.
4. Approve the `public-release` environment deployment if environment reviewers are configured.
5. Inspect the completed workflow summary and the public download URLs.

The publishing job verifies that the selected run belongs to this repository, used `.github/workflows/release.yml`, completed successfully, and built the exact supplied source commit. It downloads artifacts from that run only. It checks both checksums, metadata, safe archive entries, and ELF CPU architecture without executing binaries in the credentialed job.

The publisher uses the official AWS CLI v2 preinstalled on GitHub's `ubuntu-24.04` hosted runner, and fails with a clear error if it is missing or incompatible. It uses the R2 S3 endpoint over HTTPS with `region=auto`. The website workflow uses the explicit Wrangler version `4.72.0`; GitHub actions use commit pins. Review these tool versions during maintenance.

For version `0.2.0`, the public objects are:

```text
https://downloads.birdapp.dev/releases/v0.2.0/crow-v0.2.0-linux-x64.tar.gz
https://downloads.birdapp.dev/releases/v0.2.0/crow-v0.2.0-linux-arm64.tar.gz
https://downloads.birdapp.dev/releases/v0.2.0/SHA256SUMS
https://downloads.birdapp.dev/latest.txt
```

`latest.txt` contains exactly `0.2.0` followed by a newline. No JSON parser is required on the installing machine. Stable versions have three numeric components, with no `v`, prerelease suffix, or leading zeroes.

Before writing anything, publication compares any existing versioned objects with the candidate bytes. It refuses different bytes under an existing version. Conditional writes prevent replacement if another writer creates an object meanwhile. After uploading missing objects, it downloads every file through the public domain and checks the bytes. It updates `latest.txt` last and verifies that public URL too. Publications are serialized and cannot cancel one another midway through a run.

If an upload fails, the previous `latest.txt` remains until all release files verify. Rerun the same build to fill missing objects. If the public domain is not ready, fix the domain or caching problem and rerun; do not change the versioned objects. The script refuses to move `latest.txt` to an older version. To withdraw a bad release, prepare a corrected newer version or perform a separately reviewed rollback procedure. There is no automatic rollback publication command.

Checksums catch corruption and mixed artifacts. HTTPS and control of the distribution account establish authenticity; a checksum downloaded from the same host is not an independent signature. Signing releases can be added later without changing the installer URL.

## Deploy the installation page

Publish and verify the first hosted binary release before deploying the installation page. Then manually run **Deploy Crow installation website** from the reviewed workflow revision with confirmation `deploy birdapp.dev`. The workflow checks that the public latest version and both architecture downloads exist, then uploads only `website/` to the production Pages project. It records the exact selected workflow commit in Pages.

Check the following after deployment:

```sh
curl -fsSI https://birdapp.dev/install.sh
curl -fsSI https://downloads.birdapp.dev/latest.txt
curl -fsS https://downloads.birdapp.dev/latest.txt
```

Both mutable files must have cache headers that require a fresh response. Verify the page and installer in a clean Ubuntu VM with an interactive terminal before advertising installation. Test that a desktop browser can complete the headless machine's setup links. Do not enable a test review during setup.

Pages deployment and binary publication are independent, manual operations. Draft PRs never publish automatically. Downloading public Crow binaries does not grant access to the private source repository.

## References

- [Pages upload limits](https://developers.cloudflare.com/pages/platform/limits/)
- [Pages direct upload](https://developers.cloudflare.com/pages/get-started/direct-upload/)
- [Pages custom domains](https://developers.cloudflare.com/pages/configuration/custom-domains/)
- [R2 public custom domains](https://developers.cloudflare.com/r2/buckets/public-buckets/)
- [R2 S3 tokens](https://developers.cloudflare.com/r2/api/tokens/)
- [R2 S3 API compatibility](https://developers.cloudflare.com/r2/api/s3/api/)
