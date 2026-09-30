# Releasing OTerminal 2.x (the `rust` branch)

Every push to `rust` builds, versions and publishes OTerminal. Installed
copies find the new release on GitHub and update themselves. This file covers
how that works, what has to be set up once, and how to run the builds on your
own machine.

## Versions

| What | Where | Example |
| --- | --- | --- |
| OTerminal version (what users see, what the updater compares) | `OTERMINAL_VERSION` (repository root) | `2.0.7` |
| Zed version this fork is based on | `crates/zed/Cargo.toml` `version` | `1.21.0` |

The two are kept apart on purpose. Zed's version (`AppVersion`) is still what
extensions, remote-server downloads, the dev-env API, LSP client info and
telemetry see. Changing it would break those.

`OTERMINAL_VERSION` is the single source of truth for OTerminal's own
version. CI bumps it, and it is read at build time by:

- `crates/zed/build.rs` → `release_channel::OTerminalVersion`, shown in
  *About OTerminal* as `OTerminal 2.0.7` / `2.0.7 (based on Zed 1.21.0)`, and
  used by the updater;
- `crates/cli/build.rs` → `oterminal --version` (the CLI in `bin\`);
- `crates/windows_resources` → the exe file/product version;
- `script/bundle-windows.ps1` → the installer version.

The 2.x numbering sorts above the VS Code-based OTerminal (`1.110.x`), which
is what makes a later switch-over possible.

To test the updater against real releases, run an installed build with
`OTERMINAL_APP_VERSION=2.0.0` so it thinks it is older than it is.

## What happens on a push to `rust`

Workflow: `.github/workflows/rust-release.yml`.

1. **prepare** (ubuntu, about 1 min): skips commits whose message contains
   `[skip ci]` or `chore(release):`. Otherwise it bumps the patch number in
   `OTERMINAL_VERSION`, commits `chore(release): vX.Y.Z [skip ci]` to `rust`,
   and pushes the annotated tag `vX.Y.Z`. If that tag already exists (left by
   an earlier failed run), it moves on to the next free number.
2. **windows**: checks out the bump commit and runs
   `script/bundle-windows.ps1 -Architecture x86_64`. That builds
   `oterminal.exe`, the CLI and `auto_update_helper.exe`, then the Inno Setup
   installer. The job stages:
   - `OTerminal-X.Y.Z-windows-x86_64-setup.exe`
   - `SHA256SUMS.txt` (`sha256sum` format)
3. **release** (ubuntu): verifies the checksums and creates a GitHub
   **pre-release** with `--latest=false`. The release notes list the commits
   since the previous `v2+` tag, without the `chore(release)` commits.
4. **cleanup**: if the build or release failed, deletes the tag, so the next
   release's notes still start from the last *published* version. The bump
   commit stays, and the next run bumps past it.

Commits that only touch Markdown or `docs/` do not trigger a build.
`workflow_dispatch` offers:

- `release` off: build only. The installer is attached to the run and nothing
  is tagged.
- `bump`: `patch`, `minor` or `major`.
- `runner`: `auto`, `github-hosted` or `self-hosted`.

Pushes that arrive while a build is running wait for it (concurrency group
`oterminal-rust-release`). Only the newest waiting run is kept, so a burst of
pushes becomes one release.

macOS and Linux jobs are present but disabled (`if: false`, see the TODO in the
workflow).

## How the in-app updater works

Code: `crates/auto_update/src/auto_update.rs` and `github_release.rs`.

**Checking**
- It checks on startup and then every 4 hours, plus whenever you run *Check
  for Updates* (the OTerminal menu, or `auto update: check` in the command
  palette).
- It calls `GET https://api.github.com/repos/OverTimeHosting/Oterminal/releases`,
  unauthenticated.
- The ETag is cached for the session, so repeat checks return `304 Not
  Modified` and do not count against the 60 requests/hour limit.
- On `403`/`429` rate limits it backs off. It honours `Retry-After` or
  `X-RateLimit-Reset`, waiting between 1 minute and 6 hours. Automatic checks
  stay silent; manual ones report the wait.

**Choosing a release**
- Only tags that parse as semver with major ≥ 2 are considered. The VS Code
  line's `v1.110.x` tags are ignored.
- Drafts are skipped.
- Pre-releases are included while `"auto_update_include_prereleases": true`,
  which is the default. Every 2.x release is a pre-release for now.
- The newest remaining release that has an asset for this platform wins
  (`OTerminal-<ver>-windows-x86_64-setup.exe`), but only if it is newer than
  the running version.

**Verifying the download**
- The SHA-256 has to match GitHub's asset `digest` and/or the release's
  `SHA256SUMS.txt`.
- A release with neither, or where the two disagree, is refused.

**Installing on Windows**
- It is the same mechanism Zed uses. The installer is downloaded to
  `<install dir>\updates\` and run with:
  `/VERYSILENT /SUPPRESSMSGBOXES /NORESTART /NOCLOSEAPPLICATIONS /SP- /update=true /MERGETASKS=!desktopicon /DIR=<install dir> /LOG=...`
- In update mode `zed.iss` stages the new files in `<install dir>\install\` and
  writes `updates\versions.txt`.
- The title bar then shows **Restart to Update**. Clicking it (or quitting
  OTerminal) runs `tools\auto_update_helper.exe`, which swaps the files, with
  rollback if a step fails, and relaunches OTerminal (only after the click,
  not after a quit).
- OTerminal never restarts on its own.

**Copies that cannot update themselves**
- This covers development builds (`target\debug\oterminal.exe`), portable
  copies, and macOS/Linux for now. They show **Update Available**, and
  clicking it opens the release page.
- Debug builds do not check automatically unless `OTERMINAL_UPDATE_CHECK=1` is
  set.

**Other**
- *Help → View Release Notes* opens the GitHub release of the running (or
  pending) version.
- Logs:
  - the OTerminal log (`auto_update` lines)
  - `<install dir>\updates\OTerminal-Setup.log` (installer)
  - `<install dir>\tools\auto_update_helper.log` (file swap)
- Settings: `"auto_update": true` (default) and
  `"auto_update_include_prereleases": true` (default).

## One-time setup (repository owner)

1. **Push the workflow.** Pushing files under `.github/workflows/` needs a
   token with the `workflow` scope:
   ```sh
   gh auth refresh -h github.com -s workflow
   git push origin rust
   ```
2. **Workflow permissions.** Settings → Actions → General → *Workflow
   permissions* must let `GITHUB_TOKEN` write contents: either "Read and
   write permissions", or leave "Read" and rely on the workflow's own
   `permissions: contents: write`, as long as the organization does not
   forbid it. The job pushes the bump commit and tag to `rust` and creates
   the release. If `rust` is ever branch-protected, allow GitHub Actions to
   push to it.
3. **Manual runs (optional).** GitHub only lists `workflow_dispatch` workflows
   that exist on the default branch (`main`). To get the *Run workflow*
   button, copy `.github/workflows/rust-release.yml` to `main` unchanged. Its
   push trigger only matches `rust`, so it never runs for `main` pushes and
   does not interfere with `build.yml`.
4. **No secrets are required.** Builds are unsigned (see Risks).
5. **Repository variables (optional).** Settings → Secrets and variables →
   Actions → Variables:

   | Variable | Default | Meaning |
   | --- | --- | --- |
   | `OTERMINAL_WINDOWS_RUNNER` | `github-hosted` | `self-hosted` builds on your machine (see below) |
   | `OTERMINAL_HOSTED_WINDOWS_IMAGE` | `windows-2022` | GitHub-hosted image (2022 has VS 2022 and Inno Setup; the workflow installs Inno Setup when it is missing) |
   | `OTERMINAL_SELF_HOSTED_LABELS` | `self-hosted,Windows,X64,oterminal` | labels the self-hosted job asks for |
   | `OTERMINAL_SELF_HOSTED_TARGET_DIR` | *(drive with most space)*`\oterm-target` | persistent cargo target dir on the self-hosted runner, e.g. `D:\oterminal-ci-target` |
   | `OTERMINAL_USE_SCCACHE` | `true` | `false` disables sccache on GitHub-hosted runners |
   | `OTERMINAL_FAST_RELEASE_PROFILE` | on for hosted, off for self-hosted | `CARGO_PROFILE_RELEASE_LTO=off`, `CODEGEN_UNITS=16`, `DEBUG=0` |

## Cutting the first release

1. Commit these changes on `rust`. `OTERMINAL_VERSION` holds `2.0.0`.
2. Push them (step 1 above). The push itself triggers the workflow:
   `prepare` bumps to **2.0.1**, commits `chore(release): v2.0.1 [skip ci]`
   and tags `v2.0.1`. Then the build and pre-release follow.
3. Watch it with `gh run watch` or on the Actions tab. Afterwards run
   `git pull` on `rust` to get the bump commit.
4. Install `OTerminal-2.0.1-windows-x86_64-setup.exe` from the release by hand
   **once** on each machine. Copies built before this change still point at
   Zed's servers and cannot find the GitHub release. The installer keeps the
   same AppId, so it upgrades an existing OTerminal 2.x install in place.
5. The next push produces 2.0.2, and the 2.0.1 install offers **Restart to
   Update** within 4 hours or on its next start. *Check for Updates* finds it
   immediately.

To release a new minor or major version, run the workflow manually with
`bump: minor`/`major`, or edit `OTERMINAL_VERSION` to e.g. `2.0.99` so the
next push produces `2.0.100`. Keep it strictly `MAJOR.MINOR.PATCH`, because
Inno Setup's `VersionInfoVersion` rejects pre-release suffixes.

## Self-hosted runner (your Windows PC)

A capable desktop with a persistent target directory turns a multi-hour
hosted build into an incremental one.

1. Prerequisites:
   - the same toolchain you already build with: rustup, Visual Studio 2022
     Build Tools with the C++ workload and Windows SDK, CMake, Git;
   - **PowerShell 7** (`pwsh`), which the workflow's steps run in;
   - **Inno Setup 6** (`winget install JRSoftware.InnoSetup`, or let the
     workflow `choco install` it).
2. Repository → Settings → Actions → Runners → **New self-hosted runner** →
   Windows x64. Follow the download and `config.cmd` steps. When asked for
   labels, add `oterminal`. `self-hosted`, `Windows` and `X64` are added
   automatically. Install it as a service (`--runasservice`) or run
   `run.cmd` when you want builds.
   - Use a short work folder on a roomy drive, e.g. `D:\a`. Zed paths get
     long, so also run `git config --system core.longpaths true`.
   - The service account needs the Rust toolchain on its `PATH`. The easiest
     setup is to run the service as your own user.
3. Set the repository variables `OTERMINAL_WINDOWS_RUNNER=self-hosted` and
   `OTERMINAL_SELF_HOSTED_TARGET_DIR=D:\oterminal-ci-target`, or any folder
   outside the runner's work dir, since `actions/checkout` cleans that.
4. To go back to GitHub-hosted runners, delete the variable, or pick `runner:
   github-hosted` in a manual run. If the machine is offline, queued jobs wait
   up to 24 h. Switch the variable if you will be away.

Security: this repository is **public**. The workflow only runs on pushes to
`rust` and on manual dispatch, never on `pull_request`, so strangers' PRs
cannot run code on your machine. Keep it that way. If you add PR builds, run
them on GitHub-hosted runners only.

## Expected build times

These are estimates for this workspace (about 1,500 crates), not measured on
this workflow yet. The build step prints its own duration.

| Runner | Cold | Warm |
| --- | --- | --- |
| GitHub-hosted `windows-2022` (4 vCPU, 16 GB), fast profile, sccache | 2 – 3 h | ~1 – 1.5 h |
| GitHub-hosted, full `Cargo.toml` release profile (thin LTO, codegen-units=1) | likely > 6 h (job limit) | not viable |
| Self-hosted 16+ cores, persistent target dir, full profile | 40 – 70 min | 10 – 25 min |

Add about 5 minutes for the Rust toolchain, cargo-about and Inno Setup, and
under a minute each for prepare and release. A Zed release build needs roughly
25–40 GB of disk. The workflow puts `CARGO_TARGET_DIR` on the drive with the
most free space and removes unused SDKs if that drive is `C:` with less than
60 GB free.

## Switching the VS Code-based OTerminal over to 2.x (later)

How the VS Code line updates (`src/vs/platform/update/electron-main/githubReleaseProvider.ts`
on `main`): it calls `GET /repos/OverTimeHosting/othcloud-terminal/releases/latest`.
That is this repository's old name, which GitHub redirects. The call:

- never returns drafts or pre-releases, and the code rejects `prerelease`
  releases as well;
- updates only when the tag is newer than the running `1.110.x`;
- only takes an asset matching `/win32-x64-user-setup\.exe$/i`.

Today three things keep the two lines apart: 2.x releases are pre-releases,
they are never marked latest, and their asset names do not match. When 2.x is
ready for everyone:

1. **Stop marking 2.x as pre-release.** Drop `--prerelease` and
   `--latest=false` in the release job.
2. **Mind what that exposes.** `/releases/latest` will then return a 2.x
   release. Old VS Code-based installs see the newer version but no matching
   asset, so they log a warning and do nothing (safe, but they stay on 1.110).
3. **Move those users over** with one final VS Code-line release (`1.110.N`
   from `main`) that tells them about OTerminal 2, links to or downloads
   `OTerminal-2.x-windows-x86_64-setup.exe`, and then stops checking.
   Do **not** attach a 2.x installer named `*-win32-x64-user-setup.exe` to a
   2.x release: the VS Code updater would run it with VS Code's
   inno-updater arguments, which `zed.iss` does not understand.
4. Keep `auto_update_include_prereleases` as a setting for a future
   beta channel. Stable users can turn it off once stable releases exist.

## Risks and known limitations

- **Unsigned builds.** The first manual install shows SmartScreen's "Windows
  protected your PC". Updates are downloaded by OTerminal itself, have no
  Mark-of-the-Web and install per-user without UAC, so they are not prompted.
  To sign, wire the `AZURE_*`/`ACCOUNT_NAME`/`CERT_PROFILE_NAME`/`ENDPOINT`/`*_DIGEST`/`TIMESTAMP_SERVER`
  secrets into the build step's env; `bundle-windows.ps1` signs when they are
  all present.
- **Integrity relies on GitHub.** The SHA-256 check protects against broken
  or truncated downloads. It cannot protect against a compromised repository
  or maintainer token. Code signing plus signature verification would.
- **Failed releases.** A failed release leaves a `chore(release)` commit
  without a release. That is harmless: the tag is removed and the next run
  bumps again.
- **The hosted fast profile** (no LTO, codegen-units=16, no debug info)
  produces a somewhat larger and slightly slower binary than the full profile.
  Self-hosted builds use the full profile by default.
- **Cache pressure.** The GitHub Actions cache is 10 GB per repository and is
  shared with `main`'s npm caches. sccache entries may get evicted, making
  hosted builds slower.
- **Updating while other terminals are open.** `amd_ags_x64.dll` is not
  replaced by auto-updates because the running app may have it loaded. Its
  version is pinned, so this only matters if the AGS SDK is ever bumped (a
  full manual install replaces it). `OpenConsole.exe`, `conpty.dll` and the
  exes are swapped by `auto_update_helper.exe` after OTerminal exits. If a
  swap fails it rolls back and reports the error.
