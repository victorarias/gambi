# Release Runbook (gambi)

This file documents how agents should cut and publish a release for macOS + Linux.

## Preconditions

1. `main` is green in CI.
2. `Cargo.toml` has the intended version.
3. `README.md` install docs match current distribution flow.

## Release Steps

1. Update version:
   - Edit `Cargo.toml` `package.version`.
   - Run `cargo check --locked` to validate.
   - Commit the version bump to `main`.
2. Tag and push:
   - `git tag vX.Y.Z`
   - `git push origin vX.Y.Z`
3. Wait for `.github/workflows/release.yml` to complete.
   - It builds and uploads:
     - `gambi-x86_64-apple-darwin.tar.gz`
     - `gambi-aarch64-apple-darwin.tar.gz`
     - `gambi-x86_64-unknown-linux-gnu.tar.gz`
     - `gambi-aarch64-unknown-linux-gnu.tar.gz`
   - It also uploads:
     - `checksums.txt`
     - `homebrew-gambi.rb` (formula for the tap repo)

## Homebrew Tap Update

1. Download the formula asset from the GitHub release (`homebrew-gambi.rb`).
2. In `victorarias/homebrew-tap`, replace `Formula/gambi.rb` with that file.
3. Commit and push:
   - `git add Formula/gambi.rb`
   - `git commit -m "gambi vX.Y.Z"`
   - `git push`
4. Verify install:
   - `brew update`
   - `brew install victorarias/tap/gambi`

## Installer Script Verification

After release, validate the script path and install flow:

1. `curl -fsSL https://raw.githubusercontent.com/victorarias/gambi/main/install.sh | sh -s -- --version vX.Y.Z --bin-dir /tmp/gambi-bin`
2. `/tmp/gambi-bin/gambi --version`

## Manual Re-run

If tag automation fails, re-run from Actions with `workflow_dispatch` and pass the existing tag (for example `vX.Y.Z`).
