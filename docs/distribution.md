# npm distribution

`npm i -g ssh-clipboard` installs a JavaScript launcher plus four native Rust executables:

```text
vendor/darwin-arm64/ssh-clipboard
vendor/darwin-amd64/ssh-clipboard
vendor/linux-arm64/ssh-clipboard
vendor/linux-amd64/ssh-clipboard
```

The launcher maps Node's `process.platform` and `process.arch` to one executable, inherits the terminal unchanged for Ratatui, and forwards the native exit status. It also exports the vendor directory to the Rust process. First-run setup can therefore upload the correct native executable to a peer with a different OS or architecture.

After installation, every daemon checks npm `@latest` independently. A newer stable package is downloaded only from the canonical npm tarball URL and must pass the registry SHA-512 integrity value, the package's per-target SHA-256 manifest, native executable-header validation, and an executed version check before atomic activation. `SSH_CLIPBOARD_DISABLE_AUTO_UPDATE=1` disables this behavior for development and controlled environments; `ssh-clipboard update --check` is always read-only.

## Standalone peer installation

Standalone installations can add peers with a different OS or architecture without
the npm launcher or a manually downloaded release bundle. Setup first checks the
remote installation. If it needs a binary and none is bundled locally, setup
downloads the npm package for its **exact running version** and verifies the npm
SHA-512 integrity, per-target SHA-256 manifest, and executable architecture before
uploading. Foreign binaries are never executed on the local machine.

Verified peer binaries are cached privately under the state directory's
`peer-binaries/<version>/<target>/` tree. Cache contents are checked on reuse;
missing or corrupt entries are downloaded again. First use requires internet
access and a published package for the running version. Already-current peers
and valid cache entries do not require a download. Automatic updates do not need
to retain every platform binary.

## Building a package

The release workflow builds all four targets on their native GitHub-hosted runners. To reproduce the packaging step with a directory of flat release artifacts:

```sh
npm ci
npm test
npm run stage:binaries -- /path/to/release-binaries
npm pack
```

Staging records every binary's size and SHA-256 digest in `vendor/manifest.json`. The `prepack` hook rejects missing, non-executable, corrupt, or incorrectly formatted Mach-O/ELF payloads before npm can create the tarball.

## Publishing

The `ssh-clipboard` package name is reserved in the public npm registry by the `0.0.0-dev.ffffff` placeholder release. Replace that placeholder with the first complete release before advertising the global install command.

Configure npm trusted publishing for this GitHub repository and `.github/workflows/release.yml`, then set the GitHub repository variable `NPM_PUBLISH=true`. Version tags will build, verify, package, and publish through short-lived OIDC credentials with npm provenance; no long-lived npm token is stored in GitHub.

Releases must go through the repository helper rather than a hand-written tag:

```sh
npm run release                 # interactive stable release → npm @latest
npm run release -- patch --yes # explicit/non-interactive stable release
npm run release:dev -- patch   # vX.Y.Z-dev.<hash> → npm @dev
npm run release:nightly -- patch
npm run release -- --tag=next  # any lowercase named prerelease channel
npm run release -- --dry-run   # run every gate without changing Git or versions
```

Stable releases update the Rust manifest/lockfile and npm manifest/lockfile together, commit the version to `main`, create `vX.Y.Z`, and push the branch and tag. Prereleases use a temporary local branch, push only `vX.Y.Z-<dist-tag>.<commit-hash>`, and leave `main` on its stable version. The workflow derives the npm dist-tag from the Git tag, verifies all four source versions, builds all four native targets, validates their executable headers and checksums, and publishes idempotently through npm OIDC.

The repository metadata in `package.json` must exactly match the public GitHub repository for npm provenance. It is set to `standardagents/ssh-clipboard`. The version in `package.json` and `Cargo.toml` must match the release tag.

## macOS signing

Apple Developer ID signing and notarization are not required for this npm-installed command-line package. The npm route does not launch a downloaded `.app` bundle, installer package, or disk image through Finder. The release workflow therefore does not access an Apple developer account.

If a future release adds a standalone macOS installer or GUI, sign the final Mach-O binaries with a dedicated Developer ID Application identity and notarize that distribution artifact. Treat signing credentials as release secrets; do not put them in the npm package or repository.
