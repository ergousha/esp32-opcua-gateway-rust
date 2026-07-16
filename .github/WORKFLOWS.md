# GitHub Configuration

This directory contains GitHub-specific configurations for automated workflows and best practices.

## Files Overview

### Workflows (`.github/workflows/`)

#### `esp32-build.yml`
CI pipeline for the firmware. This is a single, bare-metal `no_std` crate
(esp-hal, `xtensa-esp32s3-none-elf`, no ESP-IDF) rather than a workspace with
a separate host-testable crate, so every job needs the espup-installed `esp`
toolchain — even `cargo check`/`clippy`/`doc` compile target-specific
register/HAL code that only exists for the Xtensa target.
- **Build**: Release build verification
- **Cargo check**: Verify the crate compiles
- **Rustfmt**: Enforce code formatting
- **Clippy**: Linting and best practices
- **Documentation**: Check doc comments and build docs
- **Check Binary Size**: Reports size via cargo-bloat (non-fatal)

Triggers on: `push` to main, `pull_request` to main

#### `security-audit.yml`
Security vulnerability scanning:
- Uses `cargo-audit` to check dependencies against the RustSec advisory database
- Scheduled weekly on Mondays

Triggers on: `push` to main, `pull_request` to main, weekly schedule

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
- Verifies the tag matches `Cargo.toml`'s `version` field before releasing
- Creates GitHub releases from git tags
- Generates changelog from commit history
- Triggered when pushing version tags (v*.*.*)

Triggers on: push tags matching `v*.*.*`

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
     - `Build for ESP32-S3`
     - `Cargo check`
     - `Rustfmt`
     - `Clippy`
     - `Documentation`
     - `security_audit (Security Audit)`
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
| esp32-build.yml | ✅ | ✅ | - | - |
| security-audit.yml | ✅ | ✅ | Weekly | - |
| zizmor.yml | ✅ (workflows only) | ✅ (workflows only) | - | - |
| stale.yml | - | - | Daily | - |
| release.yml | - | - | - | ✅ |

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
- Also checks on all PRs and pushes
- Blocks merging if vulnerabilities are found

## Release Process

To create a release:

```bash
# Tag a new version
git tag -a v1.0.0 -m "Release 1.0.0"
git push origin v1.0.0
```

GitHub Actions will automatically:
- Verify the tag matches `Cargo.toml`'s version
- Create a GitHub release
- Generate changelog from commits

## Maintenance

- Review Dependabot PRs weekly
- Monitor workflow runs for failures
- Update workflows when GitHub Actions versions update
- Check security audit results regularly
