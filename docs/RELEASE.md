# Release Checklist

## Build
- `cargo build --release --target <target>`
- Run `scripts/package_release.ps1` (Windows) or equivalent shell script

## Native Dependencies
- Place ONNX Runtime, FFmpeg, and libheif binaries under `release/libs/<platform>/`
- Ensure licenses are included if required

## Signing
- Windows: Sign with Authenticode (signtool)
- macOS: Codesign + notarize

## Artifacts
- `sms-archive(.exe)`
- `libs/` (native deps)
- `LICENSE`, `THIRD_PARTY_NOTICES`
