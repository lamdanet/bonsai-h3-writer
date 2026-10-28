# Releasing Bonsai H3 Writer

Releases are built and published by GitHub Actions. You bump the version, push a tag such as
`v0.2.0`, and `.github/workflows/release.yml` builds the Windows executables and publishes them on
the repository's **Releases** page. Nothing is uploaded from your own PC.

## What the release workflow does

On a pushed tag `vX.Y.Z` (or a manual run, see below), on a `windows-latest` runner:

1. Checks that the tag matches `version` in `Cargo.toml` (`v0.2.0` needs `version = "0.2.0"`).
   If they differ, the run stops before building and tells you how to fix it.
2. Installs the Rust version pinned in `rust-toolchain.toml` (the same as CI) and builds with
   `cargo build --release --locked`.
3. Packages with `scripts/package.ps1 -SkipBuild` (the same script you can run locally):
   - `dist/BonsaiH3Writer-X.Y.Z-windows-x64.zip` containing `bonsai-h3-writer.exe`, `bh3.exe`,
     `README.md`, `LICENSE`, `CHANGELOG.md`, `docs/CLI.md`, `docs/AGENT_GUIDE.md` and
     `docs/MCP.md` (the script fails when one of them is missing)
   - `dist/BonsaiH3Writer-X.Y.Z-windows-x64.zip.sha256` (`<hash>  <file name>`, sha256sum format)
4. Uploads both files as a workflow artifact.
5. Creates the GitHub release `vX.Y.Z` with both files attached. The release notes are the
   `## [X.Y.Z]` section of `CHANGELOG.md`, followed by GitHub's generated notes (merged pull
   requests, new contributors) and a short download note. A tag with a `-` in it (`v0.3.0-beta.1`)
   becomes a **pre-release**.

The CI workflow (`.github/workflows/ci.yml`) runs separately on every branch push and pull request:
`cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` and a release build, with the same
pinned Rust (see [Updating Rust](#updating-rust)).

## First-time setup

Do this once, in the project folder (`D:\project\bonsai-h3-writer`).

1. Install the GitHub CLI and sign in (it is also used below to watch runs; step 5 can be done
   on the website instead):

   ```powershell
   winget install --id GitHub.cli
   gh auth login
   ```

2. Make sure the code is formatted and the checks pass, exactly as CI will run them:

   ```powershell
   cargo fmt --all
   cargo clippy --all-targets --locked -- -D warnings
   cargo test --all-targets --locked
   ```

   The first command downloads the pinned Rust from `rust-toolchain.toml` if it is missing.

3. Finish the README: it shows `docs/screenshot.png` at the top, so create that file with the
   recipe under "Build from source" in `README.md` (with `BH3_CONFIG_DIR` set to a fresh folder,
   so your own settings and history are not in a public picture).

4. Make the first commit on a `main` branch:

   ```powershell
   git add -A
   git commit -m "Bonsai H3 Writer 0.1.0"
   git branch -M main
   ```

5. Create the GitHub repository and push. Pick one:

   - **With the GitHub CLI** (creates the repo, adds `origin` and pushes):

     ```powershell
     gh repo create bonsai-h3-writer --public --source . --remote origin --push --description "Write MiniMax H3 video prompts with a local Bonsai 27B"
     ```

     Use `--private` instead of `--public` if you want; release downloads are then only visible
     to people with access to the repository.

   - **On github.com**: **New repository**, name `bonsai-h3-writer`, and leave "Add a README",
     ".gitignore" and "license" unticked (the project has its own). Then:

     ```powershell
     git remote add origin https://github.com/lamdanet/bonsai-h3-writer.git
     git push -u origin main
     ```


6. Open the **Actions** tab (or run `gh run watch`): the CI workflow runs for the push and should be
   green before you release.

7. Publish 0.1.0. Change the date of `## [0.1.0]` in `CHANGELOG.md` to today first if needed
   (commit and push that), then:

   ```powershell
   git tag v0.1.0
   git push --tags
   ```

GitHub settings that matter (defaults are fine for a personal repository):

- **Settings > Actions > General > Actions permissions** must allow third-party actions
  (`dtolnay/rust-toolchain`, `Swatinem/rust-cache`, `softprops/action-gh-release`).
- The release workflow asks for `contents: write` itself. If the release step fails with
  `403` / "Resource not accessible by integration", set **Workflow permissions** to
  "Read and write permissions".

## Publishing a release

1. **Pick the version** ([Semantic Versioning](https://semver.org/)): `0.1.1` for fixes, `0.2.0` for
   new features, `0.3.0-beta.1` for a pre-release.

2. **Bump `version`** in `Cargo.toml`:

   ```toml
   [package]
   name = "bonsai-h3-writer"
   version = "0.2.0"
   ```

3. **Update `Cargo.lock`** - it records the package's own version, and the release builds with
   `--locked`, which fails when the lock file is out of date:

   ```powershell
   cargo build
   ```

4. **Update `CHANGELOG.md`**: move the entries under `## [Unreleased]` into a new section and keep
   an empty `## [Unreleased]` on top. This section becomes the release notes.

   ```markdown
   ## [Unreleased]

   ## [0.2.0] - 2026-11-15

   ### Added
   - ...
   ```

5. **Optional: build the zip locally** to check it (written to `dist\`, which git ignores):

   ```powershell
   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\package.ps1
   ```

6. **Commit, tag and push**:

   ```powershell
   git commit -am "Release v0.2.0"
   git tag v0.2.0
   git push
   git push --tags
   ```

   `git push --tags` pushes every local tag; `git push origin v0.2.0` pushes just this one.

7. **Watch the run**: `gh run watch` or the **Actions** tab. When it is green the release is live:

   ```powershell
   gh release view v0.2.0 --web
   ```

## Running the workflow manually

**Actions > Release > Run workflow** (or `gh workflow run`):

- **Use workflow from** picks the branch or tag that is built.
- **tag** empty: a **dry run**, also when you picked a tag above. The zip and `.sha256` are built
  and attached to the run as an artifact; no tag or release is created or changed. Good for
  testing a branch or a tag.
- **tag** set (for example `v0.2.0`; `0.2.0` also works): the zip is **published** as that
  release. The tag must match `version` in `Cargo.toml` on the branch you picked. A tag that does
  not exist yet is created on the commit that was built. A tag that already exists must point at
  that commit; to rebuild an existing tag, choose the tag itself under **Use workflow from > Tags**
  and enter the same tag. A rebuild replaces the release's zip and `.sha256` (the new zip has
  another hash) and rewrites its notes from `CHANGELOG.md` without GitHub's generated part, so for
  a release people may have downloaded, publish a new patch version instead.

```powershell
gh workflow run release.yml -f tag=                          # dry run of the default branch
gh workflow run release.yml --ref v0.2.0 -f tag=             # dry run of an existing tag (publishes nothing)
gh workflow run release.yml --ref main -f tag=v0.2.0         # publish main as v0.2.0
gh workflow run release.yml --ref v0.2.0 -f tag=v0.2.0       # rebuild and re-upload an existing tag
gh run download --name BonsaiH3Writer-0.2.0-windows-x64      # download the newest artifact with this name
```

A failed run can also be repeated with **Re-run jobs** on the run page; it builds the same commit.

## When something goes wrong

- **"Tag 'vX.Y.Z' does not match version ... in Cargo.toml"**: the tag was pushed before the
  version bump (or with a typo). Fix `Cargo.toml`, run `cargo build`, commit, then move the tag:

  ```powershell
  git tag -f v0.2.0
  git push -f origin v0.2.0
  ```

  Or delete the wrong tag: `git tag -d v0.2.1` and `git push --delete origin v0.2.1`.
- **"the lock file ... needs to be updated but --locked was passed"**: step 3 was skipped. Run
  `cargo build`, commit `Cargo.lock` and move the tag as above.
- **"Tag ... points to ... but this run builds ..."** (manual run): the tag already exists on
  another commit. Run the workflow from the tag, or delete the tag first.
- **A bad release is already published**: delete it together with its tag, then fix and tag again.
  If people may have downloaded it, publish a new patch version instead.

  ```powershell
  gh release delete v0.2.0 --cleanup-tag --yes
  git tag -d v0.2.0
  ```

- **CI is red on formatting**: run `cargo fmt --all`, commit and push.

## Updating Rust

`rust-toolchain.toml` pins the Rust version (`channel = "1.99.0"`). CI, the release workflow and
local `cargo` commands in the project folder all use it, so a new Rust release cannot break CI
without warning. To move to a newer Rust:

1. Change `channel` in `rust-toolchain.toml` (for example to `"1.100.0"`).
2. Run the three checks from "First-time setup", step 2 (rustup downloads the new version) and fix
   what the new compiler and clippy report.
3. Commit the new pin together with those fixes, push, and wait for green CI before the next
   release.

## Checklist

- [ ] CI is green on `main`
- [ ] `version` bumped in `Cargo.toml` and `Cargo.lock` updated (`cargo build`)
- [ ] `CHANGELOG.md` has a dated `## [X.Y.Z]` section
- [ ] `README.md` is up to date (screenshot in `docs/screenshot.png`, clone URL points at the repository)
- [ ] tag `vX.Y.Z` pushed and the Release run is green
- [ ] the zip from the Releases page extracts and `bonsai-h3-writer.exe` starts
