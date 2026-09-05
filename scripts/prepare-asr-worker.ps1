[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$repositoryRoot = Split-Path -Parent $PSScriptRoot
$manifestPath = Join-Path $repositoryRoot 'src-tauri\Cargo.toml'
$metadata = (& cargo metadata --manifest-path $manifestPath --format-version 1 --no-deps) |
    ConvertFrom-Json
if ($LASTEXITCODE -ne 0) {
    throw 'cargo metadata failed while preparing the ASR worker.'
}

$hostLine = (& rustc -vV) | Where-Object { $_ -like 'host: *' } | Select-Object -First 1
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($hostLine)) {
    throw 'Unable to determine the Rust host target.'
}
$targetTriple = $hostLine.Substring('host: '.Length).Trim()
if ($targetTriple -ne 'x86_64-pc-windows-msvc') {
    throw "The MVP ASR worker currently supports x86_64-pc-windows-msvc, not '$targetTriple'."
}

$releaseDirectory = Join-Path ([string] $metadata.target_directory) 'release'
$nativeLibraryDirectory = Join-Path ([string] $metadata.target_directory) `
    'sherpa-onnx-prebuilt\sherpa-onnx-v1.13.7-win-x64-shared-MT-Release-lib\lib'
$binaryDirectory = Join-Path $repositoryRoot 'src-tauri\binaries'
New-Item -ItemType Directory -Force -Path $binaryDirectory | Out-Null
$workerSource = Join-Path $releaseDirectory 'asr_worker.exe'
$workerDestination = Join-Path $binaryDirectory "asr_worker-$targetTriple.exe"
$runtimeLibraries = @(
    'onnxruntime.dll',
    'onnxruntime_providers_shared.dll',
    'sherpa-onnx-c-api.dll',
    'sherpa-onnx-cxx-api.dll'
)

# Tauri validates externalBin/resources while compiling this same-package worker.
$bundleTargets = @($workerDestination) + @(
    $runtimeLibraries | ForEach-Object { Join-Path $binaryDirectory $_ }
)
$bootstrapFiles = @()
foreach ($target in $bundleTargets) {
    if (-not (Test-Path -LiteralPath $target -PathType Leaf)) {
        New-Item -ItemType File -Path $target | Out-Null
        $bootstrapFiles += $target
    }
}

try {
    & cargo build --manifest-path $manifestPath --release --bin asr_worker
    if ($LASTEXITCODE -ne 0) {
        throw 'The release ASR worker build failed.'
    }
    if (-not (Test-Path -LiteralPath $workerSource -PathType Leaf)) {
        throw "The ASR worker output is missing: $workerSource"
    }
    Copy-Item -LiteralPath $workerSource -Destination $workerDestination -Force

    foreach ($library in $runtimeLibraries) {
        $source = Join-Path $nativeLibraryDirectory $library
        if (-not (Test-Path -LiteralPath $source -PathType Leaf) -or
            (Get-Item -LiteralPath $source).Length -eq 0) {
            throw "The ASR runtime library is missing: $source"
        }
        Copy-Item -LiteralPath $source -Destination (Join-Path $binaryDirectory $library) -Force
        Copy-Item -LiteralPath $source -Destination (Join-Path $releaseDirectory $library) -Force
    }
    foreach ($target in $bundleTargets) {
        if ((Get-Item -LiteralPath $target).Length -eq 0) {
            throw "The prepared ASR bundle file is empty: $target"
        }
    }
}
catch {
    foreach ($bootstrapFile in $bootstrapFiles) {
        Remove-Item -LiteralPath $bootstrapFile -Force -ErrorAction SilentlyContinue
    }
    throw
}

Write-Host "Prepared ASR worker for $targetTriple."
