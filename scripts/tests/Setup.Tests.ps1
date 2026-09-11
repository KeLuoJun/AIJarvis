[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot '..\lib\AiJarvis.Setup.psm1') -Force

$script:Passed = 0
$script:Failed = 0

function Assert-True {
    param([Parameter(Mandatory)][bool] $Condition, [Parameter(Mandatory)][string] $Message)
    if (-not $Condition) { throw $Message }
}

function Assert-Equal {
    param($Expected, $Actual, [Parameter(Mandatory)][string] $Message)
    if ($Expected -ne $Actual) { throw "$Message Expected '$Expected', got '$Actual'." }
}

function Assert-Throws {
    param([Parameter(Mandatory)][scriptblock] $Action, [Parameter(Mandatory)][string] $Message)
    $threw = $false
    try { & $Action } catch { $threw = $true }
    if (-not $threw) { throw $Message }
}

function Test-Case {
    param([Parameter(Mandatory)][string] $Name, [Parameter(Mandatory)][scriptblock] $Action)
    try {
        & $Action
        $script:Passed++
        Write-Host "[PASS] $Name"
    }
    catch {
        $script:Failed++
        Write-Host "[FAIL] $Name`: $($_.Exception.Message)"
    }
}

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))
$configPath = Join-Path $repositoryRoot 'resources\config\model.toml'
$defaultsPath = Join-Path $repositoryRoot 'resources\config\defaults.toml'
$temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) ("ai-jarvis-setup-tests-" + [Guid]::NewGuid().ToString('N'))
[void](New-Item -ItemType Directory -Path $temporaryRoot)

$passToolchain = {
    @(New-AiJarvisCheck fixture_toolchain toolchain static pass 'Fixture toolchain passed.')
}
$passWindows = {
    @(New-AiJarvisCheck fixture_windows windows os_api pass 'Fixture Windows probe passed.')
}
$passNetwork = {
    @(New-AiJarvisCheck fixture_network network static pass 'Fixture network probe passed.')
}
$passLlama = {
    New-AiJarvisCheck llama model static pass 'Fixture llama passed.'
}

try {
    Test-Case 'pinned model manifest is valid' {
        $config = Read-AiJarvisToml $configPath
        Assert-AiJarvisModelConfig $config
        Assert-Equal 675569344 (Get-AiJarvisConfigValue $config 'qwen.mmproj.size_bytes') 'Pinned mmproj size changed.'
        Assert-Equal 'b96806d96061049a5b574269b049bf6241d63d46' (Get-AiJarvisConfigValue $config 'llama.commit') 'Pinned llama commit changed.'
    }

    Test-Case 'TOML parser rejects duplicate keys' {
        Assert-Throws { ConvertFrom-AiJarvisToml "value = 1`nvalue = 2" } 'Duplicate TOML keys were accepted.'
    }

    Test-Case 'mirror host lookalike is rejected' {
        $config = Read-AiJarvisToml $configPath
        $config['qwen']['model']['primary_url'] = $config['qwen']['model']['primary_url'].Replace('hf-mirror.com/', 'hf-mirror.com.evil/')
        Assert-Throws { Assert-AiJarvisModelConfig $config } 'A mirror lookalike host was accepted.'
    }

    Test-Case 'official source cannot replace the primary mirror' {
        $config = Read-AiJarvisToml $configPath
        $config['qwen']['model']['primary_url'] = $config['qwen']['model']['fallback_url']
        Assert-Throws { Assert-AiJarvisModelConfig $config } 'The official source was accepted as the primary source.'
    }

    Test-Case 'official fallback host lookalike is rejected' {
        $config = Read-AiJarvisToml $configPath
        $config['qwen']['model']['fallback_url'] = $config['qwen']['model']['fallback_url'].Replace('huggingface.co/', 'huggingface.co.evil/')
        Assert-Throws { Assert-AiJarvisModelConfig $config } 'An official-source lookalike host was accepted.'
    }

    Test-Case 'mutable model revision is rejected' {
        $config = Read-AiJarvisToml $configPath
        $config['qwen']['revision'] = 'main'
        Assert-Throws { Assert-AiJarvisModelConfig $config } 'A mutable model revision was accepted.'
    }

    Test-Case 'model server cannot bind beyond loopback' {
        $config = Read-AiJarvisToml $configPath
        $config['server']['host'] = '0.0.0.0'
        Assert-Throws { Assert-AiJarvisModelConfig $config } 'A public unauthenticated model bind was accepted.'
    }

    Test-Case 'unsafe runtime defaults are rejected' {
        $defaults = Read-AiJarvisToml $defaultsPath
        $defaults['screen']['frame_ring_capacity'] = 0
        Assert-Throws { Assert-AiJarvisDefaultsConfig $defaults } 'A zero frame-ring capacity was accepted.'
        $defaults = Read-AiJarvisToml $defaultsPath
        $defaults['chat']['evict_to_context_ratio'] = 0.9
        Assert-Throws { Assert-AiJarvisDefaultsConfig $defaults } 'Inverted chat eviction ratios were accepted.'
    }

    Test-Case 'unsafe model runtime values are rejected' {
        $config = Read-AiJarvisToml $configPath
        $config['qwen']['sampling']['temperature'] = -1.0
        Assert-Throws { Assert-AiJarvisModelConfig $config } 'A negative sampling temperature was accepted.'
        $config = Read-AiJarvisToml $configPath
        $config['sensevoice']['rust_crate_version'] = 'latest'
        Assert-Throws { Assert-AiJarvisModelConfig $config } 'A mutable SenseVoice runtime version was accepted.'
        $config = Read-AiJarvisToml $configPath
        $config['sensevoice']['sample_rate_hz'] = 8000
        Assert-Throws { Assert-AiJarvisModelConfig $config } 'An incompatible SenseVoice sample rate was accepted.'
    }

    Test-Case 'WebView2 probe tolerates missing optional path variables' {
        $programFilesX86 = ${env:ProgramFiles(x86)}
        $localAppData = $env:LOCALAPPDATA
        try {
            Remove-Item 'Env:ProgramFiles(x86)' -ErrorAction SilentlyContinue
            Remove-Item 'Env:LOCALAPPDATA' -ErrorAction SilentlyContinue
            $probe = Get-AiJarvisWebView2Probe
            Assert-True ($null -ne $probe) 'WebView2 probe did not return a result.'
        }
        finally {
            if ($null -ne $programFilesX86) { ${env:ProgramFiles(x86)} = $programFilesX86 }
            if ($null -ne $localAppData) { $env:LOCALAPPDATA = $localAppData }
        }
    }

    Test-Case 'verified WebView2 executable has matching version and content' {
        $probe = Get-AiJarvisWebView2Probe
        if ($probe.executable_verified) {
            Assert-True ($probe.selected.executable_size_bytes -gt 0) 'An empty WebView2 executable was accepted.'
            Assert-Equal $probe.selected.version $probe.selected.executable_file_version 'WebView2 file and registration versions differ.'
        }
    }

    Test-Case 'microphone privacy probe keeps desktop policy dimensions separate' {
        $probe = Get-AiJarvisMicrophonePrivacyProbe
        Assert-True ($null -ne $probe.PSObject.Properties['user_global']) 'Global user microphone policy is missing.'
        Assert-True ($null -ne $probe.PSObject.Properties['user_non_packaged']) 'Desktop-app microphone policy is missing.'
        Assert-True ($null -ne $probe.PSObject.Properties['machine_apps_policy']) 'Machine app microphone policy is missing.'
    }

    Test-Case 'microphone registry combinations remain nonauthoritative warnings' {
        foreach ($fixture in @(
            [pscustomobject]@{ user_global = 'Allow'; user_non_packaged = 'Allow'; machine_apps_policy = 2 },
            [pscustomobject]@{ user_global = 'Deny'; user_non_packaged = 'Deny'; machine_apps_policy = 1 }
        )) {
            $check = ConvertTo-AiJarvisMicrophonePrivacyCheck $fixture
            Assert-Equal 'warning' $check.status 'Registry-only microphone evidence became a definitive pass or fail.'
        }
    }

    Test-Case 'strict model response rejects extra JSON properties' {
        $response = [pscustomobject]@{
            success = $true
            status_code = 200
            raw = ''
            body = [pscustomobject]@{ choices = @([pscustomobject]@{ message = [pscustomobject]@{ content = '{"status":"ok","extra":1}' } }) }
        }
        Assert-Throws { Assert-AiJarvisNonThinkingResponse $response @('status') } 'An extra JSON property passed strict validation.'
    }

    Test-Case 'strict model response rejects reasoning field even when empty' {
        $response = [pscustomobject]@{
            success = $true
            status_code = 200
            raw = ''
            body = [pscustomobject]@{ choices = @([pscustomobject]@{ message = [pscustomobject]@{ content = '{"status":"ok"}'; reasoning_content = '' } }) }
        }
        Assert-Throws { Assert-AiJarvisNonThinkingResponse $response @('status') } 'An empty reasoning_content field passed non-thinking validation.'
    }

    Test-Case 'vision preflight requires ordered image-derived poses' {
        $fixture = [IO.File]::ReadAllText((Join-Path $repositoryRoot 'resources\prompts\probes\qwen-preflight.json'), [Text.Encoding]::UTF8) | ConvertFrom-Json -Depth 20
        Assert-Equal 2 @($fixture.image_paths).Count 'Vision preflight does not contain two images.'
        Assert-Equal 'hands_in_pockets' $fixture.expected_poses[0] 'First image expectation is wrong.'
        Assert-Equal 'arms_at_sides' $fixture.expected_poses[1] 'Second image expectation is wrong.'
        Assert-True ($fixture.vision_response_schema.properties.PSObject.Properties.Name -contains 'poses') 'Vision schema does not require pose evidence.'
    }

    Test-Case 'llama argv preserves special path boundaries' {
        $config = Read-AiJarvisToml $configPath
        $specialRoot = Join-Path $temporaryRoot 'models with spaces & symbols'
        $arguments = @(Get-AiJarvisLlamaArguments $config $specialRoot -NoUi)
        Assert-Equal 'serve' $arguments[0] 'The llama subcommand is wrong.'
        Assert-Equal '--model' $arguments[1] 'The model switch is missing.'
        Assert-True ($arguments[2].Contains('spaces & symbols')) 'The model path was flattened or escaped incorrectly.'
        Assert-Equal '--mmproj' $arguments[3] 'The mmproj switch is missing.'
        Assert-Equal '--reasoning' $arguments[5] 'The reasoning switch is missing.'
        Assert-Equal 'off' $arguments[6] 'Reasoning is not disabled.'
        Assert-True ($arguments -contains '--no-ui') 'The application-mode no-ui switch is missing.'
        Assert-True (-not (($arguments -join ' ') -match '(?i)enable_thinking|--model-url|--mmproj-url|\s-hf\s')) 'Forbidden remote or thinking arguments were generated.'
    }

    Test-Case 'same-size artifact tampering fails SHA-256' {
        $path = Join-Path $temporaryRoot 'same-size.bin'
        [IO.File]::WriteAllBytes($path, [byte[]](1, 2, 3))
        $expectedPath = Join-Path $temporaryRoot 'expected.bin'
        [IO.File]::WriteAllBytes($expectedPath, [byte[]](3, 2, 1))
        $expectedHash = (Get-FileHash -LiteralPath $expectedPath -Algorithm SHA256).Hash.ToLowerInvariant()
        $artifact = [pscustomobject]@{ Id = 'fixture'; Path = $path; SizeBytes = 3; Sha256 = $expectedHash; Url = 'https://hf-mirror.com/x' }
        Assert-Equal 'fail' (Test-AiJarvisArtifact $artifact).status 'Same-size tampering passed integrity validation.'
    }

    Test-Case 'artifact size mismatch still computes and reports SHA-256' {
        $path = Join-Path $temporaryRoot 'wrong-size.bin'
        [IO.File]::WriteAllBytes($path, [byte[]](1, 2, 3, 4))
        $artifact = [pscustomobject]@{ Id = 'fixture'; Path = $path; SizeBytes = 3; Sha256 = ('0' * 64); Url = 'https://hf-mirror.com/x' }
        $check = Test-AiJarvisArtifact $artifact
        Assert-Equal 'fail' $check.status 'Wrong-size data passed integrity validation.'
        Assert-True (-not [string]::IsNullOrWhiteSpace([string]$check.details.actual_sha256)) 'SHA-256 was not computed after a size mismatch.'
    }

    Test-Case 'complete injected fixture exits zero and preflights once' {
        $counter = @{ Value = 0 }
        $artifactProbe = { param($artifact) New-AiJarvisCheck $artifact.Id model static pass 'Fixture artifact passed.' }
        $preflight = {
            param($config, $root, $llama)
            $counter.Value++
            New-AiJarvisCheck qwen_preflight model runtime_smoke pass 'Fixture preflight passed.'
        }
        $report = Invoke-AiJarvisSetup -ConfigPath $configPath -DefaultsPath $defaultsPath -ModelRoot $temporaryRoot `
            -ToolchainProbe $passToolchain -WindowsProbe $passWindows -NetworkProbe $passNetwork `
            -LlamaProbe $passLlama -ArtifactProbe $artifactProbe -PreflightProbe $preflight
        Assert-Equal 0 $report.exit_code 'Complete fixture returned a nonzero exit code.'
        Assert-Equal 1 $counter.Value 'Preflight invocation count is wrong.'
    }

    Test-Case 'empty model root lists every missing artifact and skips preflight' {
        $emptyRoot = Join-Path $temporaryRoot 'empty'
        [void](New-Item -ItemType Directory -Path $emptyRoot)
        $counter = @{ Value = 0 }
        $preflight = { $counter.Value++; New-AiJarvisCheck qwen_preflight model runtime_smoke pass 'Should not run.' }
        $report = Invoke-AiJarvisSetup -ConfigPath $configPath -DefaultsPath $defaultsPath -ModelRoot $emptyRoot `
            -ToolchainProbe $passToolchain -WindowsProbe $passWindows -NetworkProbe $passNetwork `
            -LlamaProbe $passLlama -PreflightProbe $preflight
        Assert-Equal 1 $report.exit_code 'Missing artifacts returned the wrong exit code.'
        Assert-Equal 4 @($report.checks | Where-Object { $_.id -in @('qwen_model', 'qwen_mmproj', 'sensevoice_model', 'sensevoice_tokens') -and $_.status -eq 'fail' }).Count 'Not every missing artifact was listed.'
        Assert-Equal 0 $counter.Value 'Preflight ran with missing Qwen artifacts.'
    }

    Test-Case 'missing mmproj prevents preflight' {
        $counter = @{ Value = 0 }
        $artifactProbe = {
            param($artifact)
            $status = if ($artifact.Id -eq 'qwen_mmproj') { 'fail' } else { 'pass' }
            New-AiJarvisCheck $artifact.Id model static $status 'Fixture artifact result.'
        }
        $preflight = { $counter.Value++; New-AiJarvisCheck qwen_preflight model runtime_smoke pass 'Should not run.' }
        $report = Invoke-AiJarvisSetup -ConfigPath $configPath -DefaultsPath $defaultsPath -ModelRoot $temporaryRoot `
            -ToolchainProbe $passToolchain -WindowsProbe $passWindows -NetworkProbe $passNetwork `
            -LlamaProbe $passLlama -ArtifactProbe $artifactProbe -PreflightProbe $preflight
        Assert-Equal 1 $report.exit_code 'Missing mmproj returned the wrong exit code.'
        Assert-Equal 0 $counter.Value 'Preflight ran without mmproj.'
    }

    Test-Case 'valid Qwen still preflights when SenseVoice is missing' {
        $counter = @{ Value = 0 }
        $artifactProbe = {
            param($artifact)
            $status = if ($artifact.Group -eq 'qwen') { 'pass' } else { 'fail' }
            New-AiJarvisCheck $artifact.Id model static $status 'Fixture artifact result.'
        }
        $preflight = { $counter.Value++; New-AiJarvisCheck qwen_preflight model runtime_smoke pass 'Fixture preflight passed.' }
        $report = Invoke-AiJarvisSetup -ConfigPath $configPath -DefaultsPath $defaultsPath -ModelRoot $temporaryRoot `
            -ToolchainProbe $passToolchain -WindowsProbe $passWindows -NetworkProbe $passNetwork `
            -LlamaProbe $passLlama -ArtifactProbe $artifactProbe -PreflightProbe $preflight
        Assert-Equal 1 $report.exit_code 'Missing SenseVoice did not keep the overall gate open.'
        Assert-Equal 1 $counter.Value 'Valid Qwen was not preflighted.'
    }

    Test-Case 'failed Qwen preflight returns nonzero' {
        $artifactProbe = { param($artifact) New-AiJarvisCheck $artifact.Id model static pass 'Fixture artifact passed.' }
        $preflight = { New-AiJarvisCheck qwen_preflight model runtime_smoke fail 'Fixture preflight failed.' }
        $report = Invoke-AiJarvisSetup -ConfigPath $configPath -DefaultsPath $defaultsPath -ModelRoot $temporaryRoot `
            -ToolchainProbe $passToolchain -WindowsProbe $passWindows -NetworkProbe $passNetwork `
            -LlamaProbe $passLlama -ArtifactProbe $artifactProbe -PreflightProbe $preflight
        Assert-Equal 1 $report.exit_code 'A failed preflight returned zero.'
    }

    Test-Case 'probe errors use exit code two' {
        $errorWindows = { @(New-AiJarvisCheck fixture_windows windows os_api error 'Fixture probe error.') }
        $artifactProbe = { param($artifact) New-AiJarvisCheck $artifact.Id model static pass 'Fixture artifact passed.' }
        $preflight = { New-AiJarvisCheck qwen_preflight model runtime_smoke pass 'Fixture preflight passed.' }
        $report = Invoke-AiJarvisSetup -ConfigPath $configPath -DefaultsPath $defaultsPath -ModelRoot $temporaryRoot `
            -ToolchainProbe $passToolchain -WindowsProbe $errorWindows -NetworkProbe $passNetwork `
            -LlamaProbe $passLlama -ArtifactProbe $artifactProbe -PreflightProbe $preflight
        Assert-Equal 2 $report.exit_code 'A probe error returned the wrong exit code.'
    }

    Test-Case 'download runner resolves exactly one curl executable' {
        $downloadScript = [IO.File]::ReadAllText((Join-Path $repositoryRoot 'scripts\download-models.ps1'), [Text.Encoding]::UTF8)
        Assert-True ($downloadScript -match 'Get-Command curl\.exe[^\r\n]+Select-Object -First 1') 'The download runner can flatten multiple curl paths into one invalid command.'
        Assert-True ($downloadScript -match 'ProcessStartInfo' -and $downloadScript -match 'ArgumentList\.Add') 'curl is not launched with independent argv.'
        Assert-True ($downloadScript -match 'Move-InvalidPartial') 'Invalid resumable downloads are not quarantined.'
        Assert-True ($downloadScript -match 'Primary mirror returned data that failed size/SHA-256 verification') 'Mirror integrity failures do not trigger the official fallback.'
        $primaryIndex = $downloadScript.IndexOf("Url = `$artifact.PrimaryUrl")
        $fallbackIndex = $downloadScript.IndexOf("Url = `$artifact.FallbackUrl")
        Assert-True ($primaryIndex -ge 0 -and $fallbackIndex -gt $primaryIndex) 'The official fallback is not strictly after the mirror attempt.'
        Assert-True ($downloadScript -match 'if \(-not \$PrimaryOnly\)') 'The fallback cannot be disabled for a mirror-only run.'
        $modelConfig = [IO.File]::ReadAllText($configPath, [Text.Encoding]::UTF8)
        Assert-True ($modelConfig -match 'primary_base_url = "https://hf-mirror\.com"') 'The primary mirror is not pinned.'
        Assert-True ($modelConfig -match 'fallback_base_url = "https://huggingface\.co"') 'The authorized official fallback is not pinned.'
    }
}
finally {
    $resolvedTemp = [IO.Path]::GetFullPath($temporaryRoot)
    $systemTemp = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if ($resolvedTemp.StartsWith($systemTemp, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $resolvedTemp)) {
        Remove-Item -LiteralPath $resolvedTemp -Recurse -Force
    }
}

Write-Host "Setup tests: passed=$script:Passed, failed=$script:Failed"
if ($script:Failed -gt 0) { exit 1 }
exit 0
