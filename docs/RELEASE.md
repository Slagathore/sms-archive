# Release Checklist

Releases are built by `.github/workflows/release.yml`, triggered by pushing a
tag matching `v*` (e.g. `v0.1.0`). It builds `sms-archive` (the GUI binary,
crate `sms-app`) for Windows, macOS, and Linux in a matrix job and uploads a
`dist/` folder as a workflow artifact for each target.

The CLI binary (`sms`, crate `sms-cli`) is not currently packaged by this
workflow — only the GUI binary is released. Build/package it manually with
`cargo build --release --bin sms` if needed.

## Build

```powershell
cargo build --release --target <target>
```

## Package

Locally, use `scripts/package_release.ps1` (PowerShell 7):

```powershell
pwsh scripts/package_release.ps1 -Target x86_64-pc-windows-msvc -BinName sms-archive.exe -OutDir release
```

This copies, into `-OutDir`:

- The built `sms-archive(.exe)` binary
- The ONNX Runtime shared library (`onnxruntime.dll` / `libonnxruntime.so*` /
  `libonnxruntime*.dylib`), found by recursively searching `target/` — the
  `ort` crate downloads it at build time and its exact location varies by
  version/platform
- `LICENSE-MIT`, `LICENSE-APACHE`, `THIRD_PARTY_NOTICES`, `README.md`

In CI, the equivalent work is split across the `Package`, `Bundle native
libraries`, and `Include licenses and third-party notices` steps of the
`build` job in `release.yml`, so the same set of files ends up in the
uploaded artifact for every target in the matrix.

## Native Dependencies

- **ONNX Runtime** is bundled automatically (see above) — it ships next to
  the executable so a clean machine can run CLIP/NSFW inference without a
  separate install.
- **FFmpeg**, **Tesseract**, and **libheif** are *not* bundled. The app
  shells out to the `ffmpeg` and `tesseract` executables at runtime (see
  `crates/media/src/lib.rs` and `crates/app/src/main.rs`), and libheif is an
  optional system library. These are large and/or under their own licenses
  (LGPL/GPL for ffmpeg, depending on build; Apache-2.0 for Tesseract;
  LGPL-3.0 for libheif) — see `THIRD_PARTY_NOTICES`. Users install them
  separately and put them on `PATH` (see the README Prerequisites table).
  The release workflow logs (informationally only) whether the CI runner
  happens to have `ffmpeg`/`tesseract` on `PATH`, but never bundles them.

## Signing

Both signing steps are gated on the presence of the relevant secrets and
**skip cleanly** (they don't fail the build) when secrets are absent — the
job just produces an unsigned/unnotarized artifact and logs that fact.

### Windows (Authenticode)

Handled by the `Sign (Windows)` step in `release.yml`, using `signtool`
(located dynamically under the Windows SDK on the runner) and the DigiCert
public timestamp server.

Required secrets:

| Secret                 | Description                                             |
| ----------------------- | -------------------------------------------------------- |
| `WINDOWS_CERT_BASE64`   | Base64-encoded `.pfx` code-signing certificate            |
| `WINDOWS_CERT_PASSWORD` | Password protecting the `.pfx`                            |

Encode a `.pfx` for the secret:

```powershell
[Convert]::ToBase64String((Get-Content cert.pfx -AsByteStream -Raw)) | Set-Clipboard
```

### macOS (codesign + notarization)

Handled by the `Notarize (macOS)` step in `release.yml`: imports the signing
certificate into a temporary keychain, codesigns the binary, submits it to
Apple's notary service with `xcrun notarytool submit --wait`, then attempts
`xcrun stapler staple` (note: stapling a bare Mach-O executable, as opposed
to a `.app`/`.pkg`/`.dmg`, isn't supported by Apple's tooling, so that step
is best-effort and a failure there doesn't fail the job — Gatekeeper falls
back to an online notarization check in that case).

Required secrets:

| Secret                   | Description                                                          |
| ------------------------- | ---------------------------------------------------------------------- |
| `APPLE_CERT_BASE64`       | Base64-encoded Developer ID Application `.p12` certificate           |
| `APPLE_CERT_PASSWORD`     | Password protecting the `.p12`                                        |
| `APPLE_SIGNING_IDENTITY`  | codesign identity, e.g. `Developer ID Application: Jane Doe (TEAMID1234)` |
| `APPLE_ID`                | Apple ID email used to authenticate with `notarytool`                |
| `APPLE_ID_PASSWORD`       | App-specific password for that Apple ID ([generate one](https://support.apple.com/en-us/102654)) |
| `APPLE_TEAM_ID`           | Apple Developer Team ID                                                |

## Artifacts

Each per-target workflow artifact (`sms-archive-<target>`) contains:

- `sms-archive(.exe)` — signed/notarized if the secrets above were present
- The ONNX Runtime shared library, next to the binary
- `LICENSE-MIT`, `LICENSE-APACHE`, `THIRD_PARTY_NOTICES`

## Third-Party Notices

`THIRD_PARTY_NOTICES` at the repo root is a manually-curated summary of the
most notable runtime components (ONNX Runtime, and the external tools the
app shells out to). The authoritative, complete inventory of every crate in
the dependency tree is generated on demand with
[`cargo-about`](https://github.com/EmbarkStudios/cargo-about), configured by
`about.toml` at the repo root:

```powershell
cargo install cargo-about
cargo about generate --workspace --format json -o third-party-licenses.json
```

This also acts as a license-policy check: `about.toml`'s `accepted` list
enumerates the SPDX identifiers considered acceptable, and `cargo about
generate` (run without `--format json`) exits non-zero if any dependency
resolves to a license outside that list.

`third-party-licenses.json` is not checked into the repository (it's fully
derived from `Cargo.lock` and can be large/change on every dependency bump);
regenerate it whenever `THIRD_PARTY_NOTICES` needs to be cross-checked or a
full audit report is needed. If you'd rather have a human-readable
HTML/text report instead of JSON, write a `.hbs` Handlebars template (see
the [cargo-about template docs](https://embarkstudios.github.io/cargo-about/cli/generate/output.html))
and pass it in place of `--format json`, e.g.
`cargo about generate --workspace about.hbs -o third-party-licenses.html`.
