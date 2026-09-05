[CmdletBinding()]
param(
    [string] $ConfigPath = (Join-Path $PSScriptRoot '..\resources\config\model.toml'),
    [string] $ModelRoot,
    [ValidateSet('All', 'Qwen', 'SenseVoice')][string] $Include = 'All',
    [switch] $PrimaryOnly
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'lib\AiJarvis.Setup.psm1') -Force

function Invoke-CurlDownload {
    param(
        [Parameter(Mandatory)][string] $Url,
        [Parameter(Mandatory)][string] $Destination
    )

    $curl = Get-Command curl.exe -CommandType Application -ErrorAction Stop | Select-Object -First 1
    $arguments = @(
        '--fail',
        '--location',
        '--retry', '3',
        '--retry-all-errors',
        '--connect-timeout', '15',
        '--continue-at', '-',
        '--output', $Destination,
        $Url
    )
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $curl.Source
    $startInfo.UseShellExecute = $false
    foreach ($argument in $arguments) { [void]$startInfo.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $started = $false
    try {
        if (-not $process.Start()) { throw 'curl.exe failed to start.' }
        $started = $true
        $process.WaitForExit()
        return $process.ExitCode
    }
    finally {
        if ($started -and -not $process.HasExited) { try { $process.Kill($true) } catch {} }
        $process.Dispose()
    }
}

function Move-InvalidPartial {
    param([Parameter(Mandatory)][string] $Path)

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { return $null }
    $quarantine = "$Path.invalid.$([DateTime]::UtcNow.ToString('yyyyMMddHHmmssfff'))"
    Move-Item -LiteralPath $Path -Destination $quarantine
    Write-Warning "Moved an invalid resumable download to $quarantine"
    return $quarantine
}

$config = Read-AiJarvisToml $ConfigPath
Assert-AiJarvisModelConfig $config
$resolvedRoot = Resolve-AiJarvisModelRoot $config $ModelRoot
$artifacts = @(Get-AiJarvisArtifactDefinitions $config $resolvedRoot | Where-Object {
    $Include -eq 'All' -or $_.Group -eq $Include.ToLowerInvariant()
})

$failed = [Collections.Generic.List[string]]::new()
foreach ($artifact in $artifacts) {
    $parent = Split-Path -Parent $artifact.Path
    [void](New-Item -ItemType Directory -Force -Path $parent)

    $existing = Test-AiJarvisArtifact $artifact
    if ($existing.status -eq 'pass') {
        Write-Host "[PASS] $($artifact.Id) is already verified."
        continue
    }
    if (Test-Path -LiteralPath $artifact.Path -PathType Leaf) {
        $quarantine = "$($artifact.Path).invalid.$([DateTime]::UtcNow.ToString('yyyyMMddHHmmss'))"
        Move-Item -LiteralPath $artifact.Path -Destination $quarantine
        Write-Warning "Moved the invalid artifact to $quarantine"
    }

    $partial = "$($artifact.Path).partial"
    if (Test-Path -LiteralPath $partial -PathType Leaf) {
        $partialCheck = Test-AiJarvisArtifact ([pscustomobject]@{
            Id = $artifact.Id; Path = $partial; SizeBytes = $artifact.SizeBytes; Sha256 = $artifact.Sha256
            PrimaryUrl = $artifact.PrimaryUrl; FallbackUrl = $artifact.FallbackUrl
        })
        if ($partialCheck.status -eq 'pass') {
            [IO.File]::Move($partial, $artifact.Path, $true)
            Write-Host "[PASS] Installed verified completed download $($artifact.Path)"
            continue
        }
        if ((Get-Item -LiteralPath $partial).Length -ge $artifact.SizeBytes) {
            [void](Move-InvalidPartial $partial)
        }
    }
    $sources = [Collections.Generic.List[object]]::new()
    $sources.Add([pscustomobject]@{ Label = 'primary mirror'; Url = $artifact.PrimaryUrl; IsPrimary = $true })
    if (-not $PrimaryOnly) {
        $sources.Add([pscustomobject]@{ Label = 'pinned Hugging Face fallback'; Url = $artifact.FallbackUrl; IsPrimary = $false })
    }

    $installed = $false
    foreach ($source in $sources) {
        Write-Host "[DOWNLOAD] $($artifact.Id) from $($source.Label): $($source.Url)"
        $downloadExit = Invoke-CurlDownload $source.Url $partial
        if ($downloadExit -ne 0) {
            if ($source.IsPrimary -and -not $PrimaryOnly) {
                if ((Test-Path -LiteralPath $partial -PathType Leaf) -and (Get-Item -LiteralPath $partial).Length -ge $artifact.SizeBytes) {
                    [void](Move-InvalidPartial $partial)
                }
                Write-Warning "Primary mirror failed for $($artifact.Id) with curl exit $downloadExit. Switching explicitly to the pinned Hugging Face URL."
                continue
            }
            $failed.Add("$($artifact.Id): $($source.Label) failed with curl exit $downloadExit; resumable partial is $partial")
            break
        }

        $partialArtifact = [pscustomobject]@{
            Id = $artifact.Id
            Path = $partial
            SizeBytes = $artifact.SizeBytes
            Sha256 = $artifact.Sha256
            PrimaryUrl = $artifact.PrimaryUrl
            FallbackUrl = $artifact.FallbackUrl
        }
        $integrity = Test-AiJarvisArtifact $partialArtifact
        if ($integrity.status -eq 'pass') {
            [IO.File]::Move($partial, $artifact.Path, $true)
            Write-Host "[PASS] Installed and verified $($artifact.Path)"
            $installed = $true
            break
        }

        $quarantine = Move-InvalidPartial $partial
        if ($source.IsPrimary -and -not $PrimaryOnly) {
            Write-Warning "Primary mirror returned data that failed size/SHA-256 verification. Invalid data is $quarantine. Switching explicitly to the pinned Hugging Face URL."
            continue
        }
        $failed.Add("$($artifact.Id): $($source.Label) data failed size/SHA-256 verification; invalid data is $quarantine")
        break
    }

    if (-not $installed -and @($failed | Where-Object { $_ -like "$($artifact.Id):*" }).Count -eq 0) {
        $failed.Add("$($artifact.Id): no download source produced a verified artifact")
    }
}

if ($failed.Count -gt 0) {
    foreach ($message in $failed) { Write-Error $message -ErrorAction Continue }
    exit 1
}
exit 0
