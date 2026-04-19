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

# TODO: Copy native libs into $OutDir\libs\<platform>
# Example:
# Copy-Item -Force .\vendor\onnxruntime\windows\onnxruntime.dll (Join-Path $OutDir "libs\windows")

Write-Host "Release package created at $OutDir"
