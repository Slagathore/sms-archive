param(
    [string]$Target = "x86_64-pc-windows-msvc",
    [string]$BinName = "sms-archive.exe",
    [string]$OutDir = "release"
)

$ErrorActionPreference = "Stop"

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $OutDir "libs") | Out-Null

$binPath = Join-Path "target" (Join-Path $Target (Join-Path "release" $BinName))
if (-Not (Test-Path $binPath)) {
    Write-Error "Binary not found: $binPath. Run cargo build --release --target $Target"
}

Copy-Item -Force $binPath (Join-Path $OutDir $BinName)

# --- Native libraries -----------------------------------------------------
# The `ort` crate (ONNX Runtime bindings) downloads a prebuilt onnxruntime
# shared library at build time and drops it somewhere under target/; the
# exact subdirectory varies by ort/onnxruntime version and platform, so we
# search for it recursively instead of hardcoding a path.
#
# The library must live next to the executable (or somewhere on PATH) for
# the OS loader to find it at runtime, so it is copied directly into
# $OutDir rather than $OutDir\libs.
$libPattern = if ($IsMacOS) {
    "libonnxruntime*.dylib"
} elseif ($IsLinux) {
    "libonnxruntime*.so*"
} else {
    # Default to Windows; $IsWindows is $true on PS7/Windows and undefined
    # (falls through here) on Windows PowerShell 5.1.
    "onnxruntime.dll"
}

$nativeLib = Get-ChildItem -Path "target" -Recurse -Filter $libPattern -ErrorAction SilentlyContinue |
    Select-Object -First 1

if ($null -ne $nativeLib) {
    Copy-Item -Force $nativeLib.FullName (Join-Path $OutDir $nativeLib.Name)
    Write-Host "Copied native library: $($nativeLib.FullName) -> $(Join-Path $OutDir $nativeLib.Name)"
} else {
    Write-Warning "Could not find '$libPattern' under target/. The packaged binary may fail to start on a machine without ONNX Runtime installed. Build with 'cargo build --release --target $Target' first."
}

# ffmpeg, tesseract, and libheif are intentionally NOT bundled here: the app
# shells out to the system `ffmpeg`/`tesseract` executables and libheif is
# an optional, separately-licensed system library (see README.md
# Prerequisites). They are large and/or licensed separately (see
# THIRD_PARTY_NOTICES), so users install them themselves and put them on
# PATH. $OutDir\libs is kept as a convention for anyone who wants to vendor
# those tools alongside the package manually; it is otherwise left empty.

# --- Licenses & docs -------------------------------------------------------
foreach ($extra in @("LICENSE-MIT", "LICENSE-APACHE", "THIRD_PARTY_NOTICES", "README.md")) {
    if (Test-Path $extra) {
        Copy-Item -Force $extra (Join-Path $OutDir $extra)
    } else {
        Write-Warning "Expected file not found, skipping: $extra"
    }
}

Write-Host "Release package created at $OutDir"
