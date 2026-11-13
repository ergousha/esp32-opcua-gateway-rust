# GitHub Configuration

This directory contains GitHub-specific configurations for automated workflows and best practices.

## Files Overview

### Workflows (`.github/workflows/`)

#### `ci.yml`
CI pipeline for the firmware. This is a single ESP-IDF-based `std` crate rather
than a workspace with a separate host-testable crate, so `clippy` and `doc`
need the espup-installed `esp` toolchain to compile target-specific code.
- **Path check**: Uses the pull request files API before checking out the code
- **Build**: Release build verification
- **Rustfmt**: Enforce code formatting
- **Clippy**: Linting and best practices
- **Documentation**: Check doc comments and build docs

Triggers on: every `pull_request` to main. One `Firmware CI` job runs all checks
on a shared runner when firmware source or build inputs change. For unrelated
changes it stops after the path check, allowing the required status to resolve
without a checkout or toolchain installation. New commits cancel older PR runs.

#### `security-audit.yml`
Security vulnerability scanning:
- Uses `cargo-audit` to check dependencies against the RustSec advisory database
- Scheduled weekly on Mondays

Triggers on: pushes and pull requests to main when `Cargo.toml`, `Cargo.lock`, or
the audit workflow changes, plus the weekly schedule. Unrelated changes do not
start a runner, and new commits cancel older runs.

#### `zizmor.yml`
Static analysis of the workflow files themselves (`.github/workflows/`), to
catch injection risks, missing `persist-credentials: false`, overly broad
permissions, etc.

Triggers on: `push`/`pull_request` to main, only when `.github/workflows/**` changes

#### `stale.yml`
Marks inactive issues and PRs as stale, and closes them after a grace period.

Triggers on: daily schedule

#### `release.yml`
Every push to `main` calls the platform's reusable release workflow
(`ergousha/iot-device-sdk/.github/workflows/firmware-release.yml`, pinned by
commit SHA; Dependabot bumps it):
- **version**: the next semver from the conventional commits since the last
  `v*` tag. Only `feat:` (minor) and `fix:` (patch) release; a breaking change
  is a minor bump while below 1.0.
- **build**: no secrets, no AWS, no cache. `app.bin`, `factory.bin` (first
  flash), `manifest.json`, an SBOM; the slot-size check and the image scan.
- **publish** (Environment `release`): signs the manifest, tags `vX.Y.Z`,
  creates a GitHub release with the images (not marked Latest; its GitHub
  status does not affect the rollout), and uploads them to
  `s3://<bucket>/releases/gateway/X.Y.Z/` through the OIDC role.
- **notify**: on failure, an issue "release vX.Y.Z failed".

The platform's OTA controller takes it from there: canary, soak, fleet, with
an email at each step. There is no release PR and no GitHub App.

### Automation

#### `dependabot.yml`
Automated dependency updates:
- **Cargo**: Updates Rust dependencies weekly on Mondays
- **GitHub Actions**: Updates workflow action versions weekly

Auto-generated PRs with labels `dependencies` and appropriate ecosystem tags.

### Configuration Files

#### `CODEOWNERS`
Defines code ownership and review requirements:
- `eakin` is owner of all files by default
- Required for PR reviews

#### `pull_request_template.md`
Template shown when creating pull requests:
- Enforces consistent PR descriptions
- Includes checklist for code quality

## Setup Instructions

### 1. Branch Protection Rules

Configure on GitHub via **Settings → Branches → Branch protection rules**:

**For `main` branch:**

1. Require a pull request before merging:
   - ✅ Require approvals (1 minimum)
   - ✅ Require status checks to pass before merging:
       - `Firmware CI`
   - ✅ Require branches to be up to date before merging
   - ✅ Require code reviews before merging
   - ✅ Require approval of the most recent reviewable push
   - ✅ Require status checks to pass before merging
   - ✅ Dismiss stale pull request approvals when new commits are pushed
   - ✅ Require conversation resolution before merging

2. Include administrators: Consider whether to enforce on admins

3. Restrictions: Optional - restrict who can push to main

### 2. Enable Dependabot

1. Go to **Settings → Code security & analysis**
2. Enable **Dependabot alerts**
3. Enable **Dependabot security updates**
4. Enable **Dependabot version updates** (uses `dependabot.yml`)

### 3. Configure CODEOWNERS

1. The `.github/CODEOWNERS` file is already created
2. Go to **Settings → Branches → Branch protection rules → main**
3. ✅ Enable "Require code reviews from Code Owners"

### 4. Pull Request Settings

1. Go to **Settings → General**
2. Under "Pull Requests":
   - ✅ Allow auto-merge
   - ✅ Allow squash merging
   - ✅ Allow rebase merging

### 5. Actions Settings

1. Go to **Settings → Actions → General**
2. Under "Workflow permissions":
   - Select "Read and write permissions"
   - ✅ Allow GitHub Actions to create and approve pull requests

## Workflow Triggers

| Workflow | Push | PR | Schedule | Tag |
|----------|------|----|-----------|----|
| ci.yml | - | ✅ (firmware steps conditional) | - | - |
| security-audit.yml | ✅ (dependency paths) | ✅ (dependency paths) | Weekly | - |
| zizmor.yml | ✅ (workflows only) | ✅ (workflows only) | - | - |
| stale.yml | - | - | Daily | - |
| release.yml | ✅ (main, every push) | - | - | - |

## Local Development

Most checks require the espup-installed `esp` toolchain (see the README's
"Toolchain setup"). After `. ~/export-esp.sh`:

```bash
# Format check
cargo fmt --all -- --check

# Clippy
cargo clippy --all-targets --all-features -- -D warnings

# Documentation
cargo doc --no-deps --document-private-items

# Build
cargo build --release
```

## Dependabot Configuration

Dependabot creates weekly PRs for:
- Direct and indirect Rust dependencies
- GitHub Actions

PRs are labeled with `dependencies` tag and assigned to `eakin`.

## Security Scanning

- Runs weekly security audits via `cargo-audit`
- Also checks pushes and pull requests that change Cargo dependency manifests

## Release Process

Merge a pull request whose title is a conventional commit: `fix: ...` releases
a patch, `feat: ...` a minor version, and anything else (`docs:`, `chore:`,
`ci:`, `refactor:`) nothing. Dependabot's cargo bumps are `fix(deps):`, so a
merged dependency update ships; its GitHub Actions bumps are `ci:` and do not.
Every release goes to the canary first; the fleet follows after the soak
without a click. `iot-platform/scripts/ota.sh status gateway` shows where it
is; `hold`, `promote`, `rerun` and `rollback` are the overrides.

### One-time setup

1. Repository variables, from the iot-platform Terraform outputs:
   ```bash
   gh variable set AWS_ROLE_ARN --body "$(terraform -chdir=../iot-platform-infra output -json github_actions_role_arns | jq -r .gateway)"
   gh variable set AWS_FIRMWARE_BUCKET --body "$(terraform -chdir=../iot-platform-infra output -raw firmware_bucket_name)"
   ```
2. Signing keys and Environment `release` (main only, secret
   `FIRMWARE_SIGNING_KEY`), from an iot-device-sdk checkout:
   ```bash
   scripts/signing-keys.sh ../esp32-opcua-gateway-rust
   ```
   Commit the `keys/` and `platform.toml` it writes with a `fix:` title.
3. Optional: `gh variable set IMAGE_SCAN_FORBIDDEN --body "<home SSID>"` makes
   the image scan fail on that string.
4. Remove what release-please needed: the `RELEASE_PLEASE_APP_CLIENT_ID`
   variable, the `RELEASE_PLEASE_APP_PRIVATE_KEY` secret, and the GitHub App
   itself once no repository uses it.

## Maintenance

- Review Dependabot PRs weekly
- Monitor workflow runs for failures
- Update workflows when GitHub Actions versions update
- Check security audit results regularly
