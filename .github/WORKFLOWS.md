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
Automated release creation:
- Builds the ESP32-S3 firmware and creates a commit-specific GitHub release
- Uploads the firmware image to S3 to trigger OTA delivery
- Skips documentation, Terraform, scripts, and other non-firmware changes

Triggers on: pushes to main when firmware source, build inputs, or this workflow
change

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
| release.yml | ✅ (firmware and workflow paths) | - | - | - |

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

Configure the release workflow after applying the Terraform stack:

```bash
gh variable set AWS_ROLE_ARN --body "$(terraform -chdir=terraform output -raw github_actions_role_arn)"
gh variable set AWS_FIRMWARE_BUCKET --body "$(terraform -chdir=terraform output -raw firmware_bucket_name)"
```

Merging a firmware-related change to `main` automatically builds the firmware,
creates a commit-specific GitHub release, and uploads the image to S3. Changes
outside the firmware paths do not create a release.

## Maintenance

- Review Dependabot PRs weekly
- Monitor workflow runs for failures
- Update workflows when GitHub Actions versions update
- Check security audit results regularly
