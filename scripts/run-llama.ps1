[CmdletBinding()]
param(
    [string] $ConfigPath = (Join-Path $PSScriptRoot '..\resources\config\model.toml'),
    [string] $ModelRoot,
    [string] $LlamaExecutable,
    [switch] $NoUi,
    [switch] $DryRun
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'lib\AiJarvis.Setup.psm1') -Force

try {
    $config = Read-AiJarvisToml $ConfigPath
    Assert-AiJarvisModelConfig $config
    $resolvedRoot = Resolve-AiJarvisModelRoot $config $ModelRoot
    $executable = Resolve-AiJarvisLlamaExecutable $config $LlamaExecutable
    if ([string]::IsNullOrWhiteSpace($executable)) {
        $executable = [string](Get-AiJarvisConfigValue $config 'llama.executable')
    }
    $arguments = @(Get-AiJarvisLlamaArguments $config $resolvedRoot -NoUi:$NoUi)

    if ($DryRun) {
        [pscustomobject][ordered]@{
            executable = $executable
            arguments = $arguments
        } | ConvertTo-Json -Depth 4
        exit 0
    }

    $llamaCheck = Test-AiJarvisLlama $config $executable
    if ($llamaCheck.status -ne 'pass') { throw $llamaCheck.message }
    $qwenArtifacts = @(Get-AiJarvisArtifactDefinitions $config $resolvedRoot | Where-Object Group -eq 'qwen')
    foreach ($artifact in $qwenArtifacts) {
        $check = Test-AiJarvisArtifact $artifact
        if ($check.status -ne 'pass') { throw $check.message }
    }

    & $executable @arguments
    exit $LASTEXITCODE
}
catch {
    Write-Error $_.Exception.Message
    exit 1
}
