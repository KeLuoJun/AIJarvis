[CmdletBinding()]
param(
    [string] $ConfigPath = (Join-Path $PSScriptRoot '..\resources\config\model.toml'),
    [string] $DefaultsPath = (Join-Path $PSScriptRoot '..\resources\config\defaults.toml'),
    [string] $ModelRoot,
    [string] $LlamaExecutable,
    [switch] $Json
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'lib\AiJarvis.Setup.psm1') -Force

try {
    $report = Invoke-AiJarvisSetup `
        -ConfigPath $ConfigPath `
        -DefaultsPath $DefaultsPath `
        -ModelRoot $ModelRoot `
        -LlamaExecutable $LlamaExecutable
}
catch {
    $report = [pscustomobject][ordered]@{
        schema_version = 1
        exit_code = 2
        model_root = $null
        checks = @(
            New-AiJarvisCheck setup model static error "Setup probe failed: $($_.Exception.Message)" 'Inspect the setup script and configuration.'
        )
    }
}

if ($Json) {
    $report | ConvertTo-Json -Depth 12
}
else {
    Write-Host 'AI Jarvis environment check'
    if ($report.model_root) { Write-Host "Model root: $($report.model_root)" }
    foreach ($check in $report.checks) {
        Write-Host ("[{0}] {1}/{2}/{3}: {4}" -f $check.status.ToUpperInvariant(), $check.category, $check.layer, $check.id, $check.message)
        if (-not [string]::IsNullOrWhiteSpace([string]$check.remediation) -and $check.status -in @('fail', 'warning', 'error')) {
            Write-Host "  Action: $($check.remediation)"
        }
    }
    $counts = $report.checks | Group-Object status | Sort-Object Name
    $summary = ($counts | ForEach-Object { "$($_.Name)=$($_.Count)" }) -join ', '
    Write-Host "Summary: $summary"
    Write-Host "Exit code: $($report.exit_code)"
}

exit $report.exit_code
