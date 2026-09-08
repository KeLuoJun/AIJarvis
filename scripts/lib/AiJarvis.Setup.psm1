Set-StrictMode -Version Latest

$script:AiJarvisRepositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..\..'))

function Remove-AiJarvisTomlComment {
    param([Parameter(Mandatory)][AllowEmptyString()][string] $Line)

    $inString = $false
    $escaped = $false
    for ($index = 0; $index -lt $Line.Length; $index++) {
        $character = $Line[$index]
        if ($inString) {
            if ($escaped) {
                $escaped = $false
                continue
            }
            if ($character -eq '\') {
                $escaped = $true
                continue
            }
            if ($character -eq '"') {
                $inString = $false
            }
            continue
        }

        if ($character -eq '"') {
            $inString = $true
        }
        elseif ($character -eq '#') {
            return $Line.Substring(0, $index)
        }
    }

    if ($inString) {
        throw 'Unterminated TOML string.'
    }
    return $Line
}

function ConvertFrom-AiJarvisTomlValue {
    param(
        [Parameter(Mandatory)][string] $Value,
        [Parameter(Mandatory)][int] $LineNumber
    )

    $trimmed = $Value.Trim()
    if ($trimmed.StartsWith('"') -and $trimmed.EndsWith('"')) {
        try {
            return ($trimmed | ConvertFrom-Json -ErrorAction Stop)
        }
        catch {
            throw "Invalid TOML string on line $LineNumber`: $($_.Exception.Message)"
        }
    }
    if ($trimmed -eq 'true') { return $true }
    if ($trimmed -eq 'false') { return $false }
    if ($trimmed -match '^-?[0-9]+$') {
        return [long]::Parse($trimmed, [Globalization.CultureInfo]::InvariantCulture)
    }
    if ($trimmed -match '^-?[0-9]+\.[0-9]+$') {
        return [double]::Parse($trimmed, [Globalization.CultureInfo]::InvariantCulture)
    }

    throw "Unsupported TOML value on line $LineNumber`: $trimmed"
}

function ConvertFrom-AiJarvisToml {
    [CmdletBinding()]
    param([Parameter(Mandatory, ValueFromPipeline)][string] $Content)

    $root = [ordered]@{}
    $current = $root
    $lineNumber = 0

    foreach ($sourceLine in ($Content -split "`r?`n")) {
        $lineNumber++
        $line = (Remove-AiJarvisTomlComment -Line $sourceLine).Trim()
        if ($line.Length -eq 0) { continue }

        if ($line -match '^\[([A-Za-z0-9_.-]+)\]$') {
            $current = $root
            foreach ($segment in $Matches[1].Split('.')) {
                if (-not $current.Contains($segment)) {
                    $current[$segment] = [ordered]@{}
                }
                elseif ($current[$segment] -isnot [Collections.IDictionary]) {
                    throw "TOML table conflicts with a value on line $lineNumber`: $segment"
                }
                $current = $current[$segment]
            }
            continue
        }

        if ($line -notmatch '^([A-Za-z0-9_-]+)\s*=\s*(.+)$') {
            throw "Unsupported TOML syntax on line $lineNumber`: $line"
        }

        $key = $Matches[1]
        if ($current.Contains($key)) {
            throw "Duplicate TOML key on line $lineNumber`: $key"
        }
        $current[$key] = ConvertFrom-AiJarvisTomlValue -Value $Matches[2] -LineNumber $lineNumber
    }

    return $root
}

function Read-AiJarvisToml {
    [CmdletBinding()]
    param([Parameter(Mandatory)][string] $Path)

    $resolved = Resolve-Path -LiteralPath $Path -ErrorAction Stop
    $content = [IO.File]::ReadAllText($resolved.Path, [Text.Encoding]::UTF8)
    return ConvertFrom-AiJarvisToml -Content $content
}

function Get-AiJarvisConfigValue {
    param(
        [Parameter(Mandatory)][Collections.IDictionary] $Config,
        [Parameter(Mandatory)][string] $Path
    )

    $value = $Config
    foreach ($segment in $Path.Split('.')) {
        if ($value -isnot [Collections.IDictionary] -or -not $value.Contains($segment)) {
            throw "Missing required configuration value '$Path'."
        }
        $value = $value[$segment]
    }
    return $value
}

function Assert-AiJarvisUri {
    param(
        [Parameter(Mandatory)][string] $Value,
        [Parameter(Mandatory)][string] $ExpectedHost,
        [Parameter(Mandatory)][string] $Name
    )

    $uri = $null
    if (-not [Uri]::TryCreate($Value, [UriKind]::Absolute, [ref] $uri)) {
        throw "$Name is not an absolute URI."
    }
    if ($uri.Scheme -ne 'https' -or $uri.IdnHost -ne $ExpectedHost -or $uri.UserInfo) {
        throw "$Name must use https://$ExpectedHost with no user info."
    }
    return $uri
}

function Assert-AiJarvisModelConfig {
    [CmdletBinding()]
    param([Parameter(Mandatory)][Collections.IDictionary] $Config)

    if ((Get-AiJarvisConfigValue $Config 'schema_version') -ne 1) {
        throw 'Unsupported model configuration schema_version.'
    }

    $primaryBase = [string](Get-AiJarvisConfigValue $Config 'download.primary_base_url')
    $fallbackBase = [string](Get-AiJarvisConfigValue $Config 'download.fallback_base_url')
    [void](Assert-AiJarvisUri $primaryBase 'hf-mirror.com' 'download.primary_base_url')
    [void](Assert-AiJarvisUri $fallbackBase 'huggingface.co' 'download.fallback_base_url')
    if ((Get-AiJarvisConfigValue $Config 'download.fallback_policy') -ne 'after_primary_failure') {
        throw "download.fallback_policy must be 'after_primary_failure'."
    }

    $llamaExecutable = [string](Get-AiJarvisConfigValue $Config 'llama.executable')
    if ([IO.Path]::GetFileName($llamaExecutable) -ne 'llama.exe') {
        throw "llama.executable must target llama.exe; legacy llama-server.exe is unsupported."
    }
    $llamaCommit = [string](Get-AiJarvisConfigValue $Config 'llama.commit')
    if ($llamaCommit -notmatch '^[0-9a-f]{40}$') {
        throw 'llama.commit must be a full immutable Git commit.'
    }
    if ([long](Get-AiJarvisConfigValue $Config 'llama.size_bytes') -le 0 -or [string](Get-AiJarvisConfigValue $Config 'llama.sha256') -notmatch '^[0-9a-fA-F]{64}$') {
        throw 'llama.exe size and SHA-256 must be pinned.'
    }
    if ((Get-AiJarvisConfigValue $Config 'server.subcommand') -ne 'serve') {
        throw "server.subcommand must be 'serve'."
    }
    if ((Get-AiJarvisConfigValue $Config 'server.reasoning') -ne 'off') {
        throw "server.reasoning must be 'off'."
    }
    if ((Get-AiJarvisConfigValue $Config 'server.host') -ne '127.0.0.1') {
        throw "server.host must be loopback-only 127.0.0.1."
    }
    if ((Get-AiJarvisConfigValue $Config 'server.port') -ne 8080) {
        throw 'server.port must match the pinned development port 8080.'
    }
    if ((Get-AiJarvisConfigValue $Config 'server.alias') -ne 'qwen3.5-4b') {
        throw "server.alias must be 'qwen3.5-4b'."
    }
    if ((Get-AiJarvisConfigValue $Config 'server.context_size') -ne 8192) {
        throw 'server.context_size must match the pinned MVP context size 8192.'
    }

    $temperature = [double](Get-AiJarvisConfigValue $Config 'qwen.sampling.temperature')
    $topP = [double](Get-AiJarvisConfigValue $Config 'qwen.sampling.top_p')
    $topK = [int](Get-AiJarvisConfigValue $Config 'qwen.sampling.top_k')
    $minP = [double](Get-AiJarvisConfigValue $Config 'qwen.sampling.min_p')
    $presencePenalty = [double](Get-AiJarvisConfigValue $Config 'qwen.sampling.presence_penalty')
    $repeatPenalty = [double](Get-AiJarvisConfigValue $Config 'qwen.sampling.repeat_penalty')
    if ($temperature -lt 0 -or $temperature -gt 2 -or $topP -le 0 -or $topP -gt 1 -or $topK -le 0 -or $minP -lt 0 -or $minP -gt 1 -or $presencePenalty -lt -2 -or $presencePenalty -gt 2 -or $repeatPenalty -le 0) {
        throw 'Qwen sampling values are outside supported bounds.'
    }

    if ((Get-AiJarvisConfigValue $Config 'sensevoice.rust_crate') -ne 'sherpa-onnx') {
        throw "sensevoice.rust_crate must be 'sherpa-onnx'."
    }
    if ([string](Get-AiJarvisConfigValue $Config 'sensevoice.rust_crate_version') -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') {
        throw 'sensevoice.rust_crate_version must be an exact semantic version.'
    }
    if ((Get-AiJarvisConfigValue $Config 'sensevoice.sample_rate_hz') -ne 16000 -or (Get-AiJarvisConfigValue $Config 'sensevoice.feature_dim') -ne 80 -or (Get-AiJarvisConfigValue $Config 'sensevoice.language') -ne 'auto' -or (Get-AiJarvisConfigValue $Config 'sensevoice.use_itn') -ne $true) {
        throw 'SenseVoice preprocessing must remain 16 kHz, 80-dimensional, auto-language, with ITN enabled.'
    }

    foreach ($revisionPath in @('qwen.revision', 'sensevoice.revision')) {
        $revision = [string](Get-AiJarvisConfigValue $Config $revisionPath)
        if ($revision -notmatch '^[0-9a-f]{40}$') {
            throw "$revisionPath must be a full immutable revision."
        }
    }

    $artifactSpecs = @(
        @{ Prefix = 'qwen.model'; Repository = 'qwen.repository'; Revision = 'qwen.revision' },
        @{ Prefix = 'qwen.mmproj'; Repository = 'qwen.repository'; Revision = 'qwen.revision' },
        @{ Prefix = 'sensevoice.model'; Repository = 'sensevoice.repository'; Revision = 'sensevoice.revision' },
        @{ Prefix = 'sensevoice.tokens'; Repository = 'sensevoice.repository'; Revision = 'sensevoice.revision' }
    )

    foreach ($spec in $artifactSpecs) {
        $prefix = $spec.Prefix
        $filename = [string](Get-AiJarvisConfigValue $Config "$prefix.filename")
        $relativePath = [string](Get-AiJarvisConfigValue $Config "$prefix.relative_path")
        $size = [long](Get-AiJarvisConfigValue $Config "$prefix.size_bytes")
        $hash = [string](Get-AiJarvisConfigValue $Config "$prefix.sha256")
        $repository = [string](Get-AiJarvisConfigValue $Config $spec.Repository)
        $revision = [string](Get-AiJarvisConfigValue $Config $spec.Revision)

        if ([IO.Path]::IsPathRooted($relativePath) -or $relativePath -match '(^|[\\/])\.\.([\\/]|$)') {
            throw "$prefix.relative_path must stay below the configured model root."
        }
        if ([IO.Path]::GetFileName($relativePath) -ne $filename) {
            throw "$prefix.filename must match its relative_path leaf."
        }
        if ($size -le 0) { throw "$prefix.size_bytes must be positive." }
        if ($hash -notmatch '^[0-9a-fA-F]{64}$') { throw "$prefix.sha256 must be SHA-256." }

        foreach ($source in @(
            @{ Field = 'primary_url'; Host = 'hf-mirror.com' },
            @{ Field = 'fallback_url'; Host = 'huggingface.co' }
        )) {
            $fieldPath = "$prefix.$($source.Field)"
            $uri = Assert-AiJarvisUri ([string](Get-AiJarvisConfigValue $Config $fieldPath)) $source.Host $fieldPath
            $expectedPath = "/$repository/resolve/$revision/$filename"
            if ($uri.AbsolutePath -cne $expectedPath) {
                throw "$fieldPath does not match the pinned repository, revision, and filename."
            }
        }
    }
}

function Assert-AiJarvisDefaultsConfig {
    [CmdletBinding()]
    param([Parameter(Mandatory)][Collections.IDictionary] $Config)

    if ((Get-AiJarvisConfigValue $Config 'schema_version') -ne 1) {
        throw 'Unsupported defaults configuration schema_version.'
    }

    $nodeMajor = [int](Get-AiJarvisConfigValue $Config 'toolchain.node_min_major')
    $pnpmMajor = [int](Get-AiJarvisConfigValue $Config 'toolchain.pnpm_min_major')
    $rustVersion = $null
    if ($nodeMajor -lt 20 -or $pnpmMajor -lt 9 -or -not [version]::TryParse([string](Get-AiJarvisConfigValue $Config 'toolchain.rust_min_version'), [ref]$rustVersion) -or $rustVersion -lt [version]'1.77.2') {
        throw 'Toolchain minimum versions are below the Tauri v2 baseline.'
    }

    $captureInterval = [int](Get-AiJarvisConfigValue $Config 'screen.capture_interval_seconds')
    $frameCapacity = [int](Get-AiJarvisConfigValue $Config 'screen.frame_ring_capacity')
    $imageMin = [int](Get-AiJarvisConfigValue $Config 'screen.request_images_min')
    $imageMax = [int](Get-AiJarvisConfigValue $Config 'screen.request_images_max')
    $observationMin = [int](Get-AiJarvisConfigValue $Config 'screen.observation_min_interval_seconds')
    $nextMin = [int](Get-AiJarvisConfigValue $Config 'screen.next_check_min_seconds')
    $nextMax = [int](Get-AiJarvisConfigValue $Config 'screen.next_check_max_seconds')
    $nextDefault = [int](Get-AiJarvisConfigValue $Config 'screen.next_check_default_seconds')
    if ($captureInterval -le 0 -or $frameCapacity -le 0 -or $frameCapacity -gt 8) { throw 'Screen capture interval and frame ring capacity must be positive, with at most 8 frames.' }
    if ($imageMin -ne 1 -or $imageMax -gt 3 -or $imageMin -gt $imageMax -or $imageMax -gt $frameCapacity) { throw 'Proactive image input must allow a single frame and at most three ordered keyframes.' }
    if ($observationMin -lt 5 -or $nextMin -lt 5 -or $nextMax -gt 18 -or $nextMin -gt $nextMax -or $nextDefault -lt $nextMin -or $nextDefault -gt $nextMax) { throw 'Observation and next-check intervals violate the 5-18 second bounds.' }
    foreach ($path in @('screen.game_interval_seconds', 'screen.media_interval_seconds', 'screen.default_interval_seconds')) {
        if ([int](Get-AiJarvisConfigValue $Config $path) -lt $captureInterval) { throw "$path cannot be shorter than the capture interval." }
    }
    if ((Get-AiJarvisConfigValue $Config 'screen.pending_observation_capacity') -ne 1) { throw 'Only one latest pending observation is allowed.' }
    if ((Get-AiJarvisConfigValue $Config 'qwen.max_concurrency') -ne 1) { throw 'Qwen must remain single-flight.' }

    $sampleRate = [int](Get-AiJarvisConfigValue $Config 'audio.sample_rate_hz')
    $channels = [int](Get-AiJarvisConfigValue $Config 'audio.channels')
    $microphoneSeconds = [int](Get-AiJarvisConfigValue $Config 'audio.microphone_max_seconds')
    $microphoneBytes = [long](Get-AiJarvisConfigValue $Config 'audio.microphone_max_bytes')
    $systemAudioSeconds = [int](Get-AiJarvisConfigValue $Config 'audio.system_audio_ring_seconds')
    if ($sampleRate -ne 16000 -or $channels -ne 1 -or $microphoneSeconds -le 0 -or $microphoneSeconds -gt 60 -or $microphoneBytes -le 0 -or $microphoneBytes -gt 2097152 -or $systemAudioSeconds -le 0 -or $systemAudioSeconds -gt 60) {
        throw 'Audio defaults exceed the 16 kHz mono, 60 second, or 2 MiB MVP bounds.'
    }

    $evictAt = [double](Get-AiJarvisConfigValue $Config 'chat.evict_at_context_ratio')
    $evictTo = [double](Get-AiJarvisConfigValue $Config 'chat.evict_to_context_ratio')
    if ($evictTo -le 0 -or $evictAt -ge 1 -or $evictTo -ge $evictAt) { throw 'Chat context eviction ratios must satisfy 0 < evict_to < evict_at < 1.' }

    $messagesMin = [int](Get-AiJarvisConfigValue $Config 'danmaku.messages_min')
    $messagesMax = [int](Get-AiJarvisConfigValue $Config 'danmaku.messages_max')
    $delayMin = [int](Get-AiJarvisConfigValue $Config 'danmaku.launch_delay_min_ms')
    $delayMax = [int](Get-AiJarvisConfigValue $Config 'danmaku.launch_delay_max_ms')
    $topRatio = [double](Get-AiJarvisConfigValue $Config 'danmaku.top_region_ratio')
    if ($messagesMin -lt 3 -or $messagesMax -gt 5 -or $messagesMin -gt $messagesMax) { throw 'Danmaku batches must contain 3-5 messages.' }
    if ($delayMin -lt 400 -or $delayMax -gt 1200 -or $delayMin -gt $delayMax) { throw 'Danmaku launch delays must remain between 400 and 1200 ms.' }
    if ($topRatio -le 0 -or $topRatio -gt 0.3) { throw 'Danmaku must stay in the top 30 percent of the display.' }
}

function Resolve-AiJarvisModelRoot {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][Collections.IDictionary] $Config,
        [string] $ModelRoot
    )

    if ([string]::IsNullOrWhiteSpace($ModelRoot)) {
        $environmentName = [string](Get-AiJarvisConfigValue $Config 'paths.model_root_env')
        $ModelRoot = [Environment]::GetEnvironmentVariable($environmentName)
    }
    if ([string]::IsNullOrWhiteSpace($ModelRoot)) {
        $ModelRoot = [Environment]::ExpandEnvironmentVariables(
            [string](Get-AiJarvisConfigValue $Config 'paths.default_model_root')
        )
    }
    if ([string]::IsNullOrWhiteSpace($ModelRoot)) {
        throw 'Unable to resolve the model root.'
    }
    return [IO.Path]::GetFullPath($ModelRoot)
}

function Join-AiJarvisModelPath {
    param(
        [Parameter(Mandatory)][string] $ModelRoot,
        [Parameter(Mandatory)][string] $RelativePath
    )

    if ([IO.Path]::IsPathRooted($RelativePath)) {
        throw "Model path must be relative: $RelativePath"
    }
    $root = [IO.Path]::GetFullPath($ModelRoot).TrimEnd([IO.Path]::DirectorySeparatorChar)
    $candidate = [IO.Path]::GetFullPath((Join-Path $root $RelativePath))
    $prefix = $root + [IO.Path]::DirectorySeparatorChar
    if (-not $candidate.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Model path escapes the configured root: $RelativePath"
    }
    return $candidate
}

function Get-AiJarvisArtifactDefinitions {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][Collections.IDictionary] $Config,
        [Parameter(Mandatory)][string] $ModelRoot
    )

    $specs = @(
        @{ Id = 'qwen_model'; Prefix = 'qwen.model'; Group = 'qwen' },
        @{ Id = 'qwen_mmproj'; Prefix = 'qwen.mmproj'; Group = 'qwen' },
        @{ Id = 'sensevoice_model'; Prefix = 'sensevoice.model'; Group = 'sensevoice' },
        @{ Id = 'sensevoice_tokens'; Prefix = 'sensevoice.tokens'; Group = 'sensevoice' }
    )

    foreach ($spec in $specs) {
        $relative = [string](Get-AiJarvisConfigValue $Config "$($spec.Prefix).relative_path")
        [pscustomobject][ordered]@{
            Id = $spec.Id
            Group = $spec.Group
            Path = Join-AiJarvisModelPath $ModelRoot $relative
            RelativePath = $relative
            SizeBytes = [long](Get-AiJarvisConfigValue $Config "$($spec.Prefix).size_bytes")
            Sha256 = ([string](Get-AiJarvisConfigValue $Config "$($spec.Prefix).sha256")).ToLowerInvariant()
            PrimaryUrl = [string](Get-AiJarvisConfigValue $Config "$($spec.Prefix).primary_url")
            FallbackUrl = [string](Get-AiJarvisConfigValue $Config "$($spec.Prefix).fallback_url")
        }
    }
}

function Resolve-AiJarvisLlamaExecutable {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][Collections.IDictionary] $Config,
        [string] $ExecutablePath
    )

    if ([string]::IsNullOrWhiteSpace($ExecutablePath)) {
        $ExecutablePath = [Environment]::GetEnvironmentVariable('AI_JARVIS_LLAMA_EXE')
    }
    if (-not [string]::IsNullOrWhiteSpace($ExecutablePath)) {
        return [IO.Path]::GetFullPath($ExecutablePath)
    }

    $commandName = [string](Get-AiJarvisConfigValue $Config 'llama.executable')
    $command = Get-Command $commandName -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $command) { return $null }
    return $command.Source
}

function Get-AiJarvisLlamaArguments {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][Collections.IDictionary] $Config,
        [Parameter(Mandatory)][string] $ModelRoot,
        [switch] $NoUi
    )

    $artifacts = @(Get-AiJarvisArtifactDefinitions $Config $ModelRoot)
    $model = $artifacts | Where-Object Id -eq 'qwen_model'
    $mmproj = $artifacts | Where-Object Id -eq 'qwen_mmproj'
    $arguments = [Collections.Generic.List[string]]::new()
    foreach ($argument in @(
        [string](Get-AiJarvisConfigValue $Config 'server.subcommand'),
        '--model', $model.Path,
        '--mmproj', $mmproj.Path,
        '--reasoning', [string](Get-AiJarvisConfigValue $Config 'server.reasoning'),
        '--alias', [string](Get-AiJarvisConfigValue $Config 'server.alias'),
        '--host', [string](Get-AiJarvisConfigValue $Config 'server.host'),
        '--port', ([string](Get-AiJarvisConfigValue $Config 'server.port')),
        '--ctx-size', ([string](Get-AiJarvisConfigValue $Config 'server.context_size'))
    )) {
        $arguments.Add($argument)
    }
    if ($NoUi) { $arguments.Add('--no-ui') }

    $forbidden = @('-hf', '--model-url', '--mmproj-url')
    foreach ($argument in $arguments) {
        if ($forbidden -contains $argument -or $argument -match '(?i)enable_thinking|huggingface\.co|hf-mirror\.com') {
            throw "Forbidden llama argument: $argument"
        }
    }
    return $arguments.ToArray()
}

function New-AiJarvisCheck {
    param(
        [Parameter(Mandatory)][string] $Id,
        [Parameter(Mandatory)][ValidateSet('toolchain', 'model', 'windows', 'network', 'runtime')][string] $Category,
        [Parameter(Mandatory)][ValidateSet('static', 'os_api', 'device', 'runtime_smoke')][string] $Layer,
        [Parameter(Mandatory)][ValidateSet('pass', 'fail', 'warning', 'deferred', 'error')][string] $Status,
        [Parameter(Mandatory)][string] $Message,
        [string] $Remediation = '',
        [object] $Details = $null
    )

    [pscustomobject][ordered]@{
        id = $Id
        category = $Category
        layer = $Layer
        status = $Status
        message = $Message
        remediation = $Remediation
        details = $Details
    }
}

function Invoke-AiJarvisNativeCapture {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string] $FilePath,
        [string[]] $ArgumentList = @(),
        [ValidateRange(100, 60000)][int] $TimeoutMilliseconds = 10000
    )

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.StandardOutputEncoding = [Text.UTF8Encoding]::new($false)
    foreach ($argument in $ArgumentList) { [void]$startInfo.ArgumentList.Add($argument) }

    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    try {
        if (-not $process.Start()) { throw "Failed to start $FilePath." }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        if (-not $process.WaitForExit($TimeoutMilliseconds)) {
            try { $process.Kill($true); $process.WaitForExit() } catch {}
            return [pscustomobject]@{ ExitCode = $null; StdOut = $stdoutTask.GetAwaiter().GetResult().Trim(); StdErr = $stderrTask.GetAwaiter().GetResult().Trim(); TimedOut = $true }
        }
        return [pscustomobject]@{ ExitCode = $process.ExitCode; StdOut = $stdoutTask.GetAwaiter().GetResult().Trim(); StdErr = $stderrTask.GetAwaiter().GetResult().Trim(); TimedOut = $false }
    }
    finally {
        $process.Dispose()
    }
}

function Test-AiJarvisTool {
    param(
        [Parameter(Mandatory)][string] $Id,
        [Parameter(Mandatory)][string] $Command,
        [Parameter(Mandatory)][string[]] $Arguments,
        [Parameter(Mandatory)][scriptblock] $VersionParser,
        [Parameter(Mandatory)][scriptblock] $VersionPredicate,
        [Parameter(Mandatory)][string] $Requirement
    )

    $resolved = Get-Command $Command -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $resolved) {
        return New-AiJarvisCheck $Id toolchain static fail "$Command was not found." "Install $Requirement and ensure it is on PATH."
    }
    try {
        $capture = Invoke-AiJarvisNativeCapture $resolved.Source $Arguments
        if ($capture.TimedOut -or $capture.ExitCode -ne 0) {
            return New-AiJarvisCheck $Id toolchain static fail "$Command could not report its version." "Repair $Requirement and retry." $capture
        }
        $text = ($capture.StdOut + "`n" + $capture.StdErr).Trim()
        $version = & $VersionParser $text
        if ($null -eq $version -or -not (& $VersionPredicate $version)) {
            return New-AiJarvisCheck $Id toolchain static fail "$Command version '$version' does not satisfy $Requirement." "Install $Requirement." @{ path = $resolved.Source; output = $text }
        }
        return New-AiJarvisCheck $Id toolchain static pass "$Command $version" '' @{ path = $resolved.Source; version = [string]$version }
    }
    catch {
        return New-AiJarvisCheck $Id toolchain static error "$Command probe failed: $($_.Exception.Message)" 'Inspect the command installation and setup probe.'
    }
}

function Test-AiJarvisArtifact {
    [CmdletBinding()]
    param([Parameter(Mandatory)] $Artifact)

    if (-not (Test-Path -LiteralPath $Artifact.Path -PathType Leaf)) {
        return New-AiJarvisCheck $Artifact.Id model static fail "Missing model artifact: $($Artifact.Path)" "Download from $($Artifact.PrimaryUrl); if that source fails, use $($Artifact.FallbackUrl). The AG-03 real-model contract remains pending."
    }

    try {
        $item = Get-Item -LiteralPath $Artifact.Path -ErrorAction Stop
        $actualHash = (Get-FileHash -LiteralPath $Artifact.Path -Algorithm SHA256 -ErrorAction Stop).Hash.ToLowerInvariant()
        $sizeMatches = $item.Length -eq $Artifact.SizeBytes
        $hashMatches = $actualHash -eq $Artifact.Sha256
        if (-not $sizeMatches -or -not $hashMatches) {
            return New-AiJarvisCheck $Artifact.Id model static fail "Model artifact integrity mismatch: $($Artifact.Path)" 'Delete the invalid file and download the pinned artifact again.' @{
                expected_size = $Artifact.SizeBytes; actual_size = $item.Length
                expected_sha256 = $Artifact.Sha256; actual_sha256 = $actualHash
            }
        }
        return New-AiJarvisCheck $Artifact.Id model static pass "Verified $($Artifact.Id)." '' @{ path = $Artifact.Path; size_bytes = $item.Length; sha256 = $actualHash }
    }
    catch {
        return New-AiJarvisCheck $Artifact.Id model static error "Unable to verify $($Artifact.Path): $($_.Exception.Message)" 'Check file access and the setup probe.'
    }
}

function Invoke-AiJarvisWindowsPowerShell51 {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string] $Script,
        [ValidateRange(100, 60000)][int] $TimeoutMilliseconds = 10000
    )

    $executable = Join-Path ([Environment]::SystemDirectory) 'WindowsPowerShell\v1.0\powershell.exe'
    if (-not (Test-Path -LiteralPath $executable -PathType Leaf)) {
        return [pscustomobject]@{ ExitCode = $null; StdOut = ''; StdErr = "Windows PowerShell 5.1 not found: $executable"; TimedOut = $false }
    }
    $preamble = @'
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object Text.UTF8Encoding $false
'@
    $encoded = [Convert]::ToBase64String(
        [Text.Encoding]::Unicode.GetBytes($preamble + [Environment]::NewLine + $Script)
    )
    return Invoke-AiJarvisNativeCapture $executable @(
        '-NoLogo', '-NoProfile', '-NonInteractive', '-EncodedCommand', $encoded
    ) $TimeoutMilliseconds
}

function Get-AiJarvisWinRtProbe {
    $childScript = @'
function Format-ProbeError {
    param($Record)
    [ordered]@{
        message = $Record.Exception.Message
        hresult = ('0x{0:X8}' -f ([int64]$Record.Exception.HResult -band 0xFFFFFFFFL))
    }
}
$result = [ordered]@{ schemaVersion = 1 }
try {
    $captureSession = [Windows.Graphics.Capture.GraphicsCaptureSession, Windows.Graphics.Capture, ContentType = WindowsRuntime]
    $result['wgc'] = [ordered]@{ isSupported = [bool]$captureSession::IsSupported(); probeError = $null }
}
catch {
    $result['wgc'] = [ordered]@{ isSupported = $null; probeError = Format-ProbeError $_ }
}
try {
    $mediaDevice = [Windows.Media.Devices.MediaDevice, Windows.Media.Devices, ContentType = WindowsRuntime]
    $audioRole = [Windows.Media.Devices.AudioDeviceRole, Windows.Media.Devices, ContentType = WindowsRuntime]
    $result['audio'] = [ordered]@{
        defaultRenderId = [string]$mediaDevice::GetDefaultAudioRenderId($audioRole::Default)
        defaultCaptureId = [string]$mediaDevice::GetDefaultAudioCaptureId($audioRole::Default)
        probeError = $null
    }
}
catch {
    $result['audio'] = [ordered]@{ defaultRenderId = $null; defaultCaptureId = $null; probeError = Format-ProbeError $_ }
}
$result | ConvertTo-Json -Compress -Depth 6
'@

    $native = Invoke-AiJarvisWindowsPowerShell51 $childScript
    if ($native.TimedOut) { throw 'WinRT probe timed out.' }
    if ($native.ExitCode -ne 0) { throw "WinRT probe failed: $($native.StdErr)" }
    try {
        $result = $native.StdOut | ConvertFrom-Json -ErrorAction Stop
    }
    catch {
        throw "WinRT probe returned invalid JSON: $($native.StdOut)"
    }
    if ($result.schemaVersion -ne 1) { throw "Unsupported WinRT probe schema: $($result.schemaVersion)" }
    return $result
}

function Get-AiJarvisWebView2Probe {
    $productId = '{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
    $subKey = "SOFTWARE\Microsoft\EdgeUpdate\Clients\$productId"
    $registrations = [Collections.Generic.List[object]]::new()
    $locations = @(
        @{ Hive = [Microsoft.Win32.RegistryHive]::LocalMachine; View = [Microsoft.Win32.RegistryView]::Registry32 },
        @{ Hive = [Microsoft.Win32.RegistryHive]::LocalMachine; View = [Microsoft.Win32.RegistryView]::Registry64 },
        @{ Hive = [Microsoft.Win32.RegistryHive]::CurrentUser; View = [Microsoft.Win32.RegistryView]::Registry32 },
        @{ Hive = [Microsoft.Win32.RegistryHive]::CurrentUser; View = [Microsoft.Win32.RegistryView]::Registry64 }
    )

    foreach ($location in $locations) {
        $baseKey = $null
        $clientKey = $null
        try {
            $baseKey = [Microsoft.Win32.RegistryKey]::OpenBaseKey($location.Hive, $location.View)
            $clientKey = $baseKey.OpenSubKey($subKey, $false)
            if ($null -eq $clientKey) { continue }
            $versionText = [string]$clientKey.GetValue('pv', $null, [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            $version = $null
            if ([string]::IsNullOrWhiteSpace($versionText) -or $versionText -eq '0.0.0.0' -or -not [version]::TryParse($versionText, [ref]$version)) {
                continue
            }
            $installRoot = [string]$clientKey.GetValue('location', $null)
            $candidates = [Collections.Generic.List[string]]::new()
            if (-not [string]::IsNullOrWhiteSpace($installRoot)) {
                $candidates.Add((Join-Path (Join-Path $installRoot $versionText) 'msedgewebview2.exe'))
                $candidates.Add((Join-Path $installRoot 'msedgewebview2.exe'))
            }
            foreach ($basePath in @(${env:ProgramFiles(x86)}, $env:LOCALAPPDATA)) {
                if ([string]::IsNullOrWhiteSpace([string]$basePath)) { continue }
                $knownRoot = Join-Path $basePath 'Microsoft\EdgeWebView\Application'
                $candidates.Add((Join-Path (Join-Path $knownRoot $versionText) 'msedgewebview2.exe'))
            }
            $verifiedExecutables = foreach ($candidate in @($candidates | Select-Object -Unique)) {
                if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) { continue }
                $item = Get-Item -LiteralPath $candidate -ErrorAction Stop
                $fileVersion = $null
                $versionMatches = [version]::TryParse($item.VersionInfo.FileVersion, [ref]$fileVersion) -and $fileVersion -eq $version
                if ($item.Length -gt 0 -and $versionMatches) {
                    [pscustomobject]@{ path = $item.FullName; size_bytes = $item.Length; file_version = [string]$fileVersion }
                }
            }
            $verifiedExecutable = @($verifiedExecutables | Select-Object -First 1)
            $registrations.Add([pscustomobject]@{
                hive = [string]$location.Hive
                view = [string]$location.View
                version = $versionText
                install_root = $installRoot
                executable_path = if ($verifiedExecutable.Count -gt 0) { $verifiedExecutable[0].path } else { $null }
                executable_size_bytes = if ($verifiedExecutable.Count -gt 0) { $verifiedExecutable[0].size_bytes } else { $null }
                executable_file_version = if ($verifiedExecutable.Count -gt 0) { $verifiedExecutable[0].file_version } else { $null }
                executable_verified = $verifiedExecutable.Count -gt 0
            })
        }
        finally {
            if ($clientKey) { $clientKey.Dispose() }
            if ($baseKey) { $baseKey.Dispose() }
        }
    }

    $usable = @($registrations | Where-Object executable_verified | Sort-Object { [version]$_.version } -Descending)
    return [pscustomobject]@{
        registered = $registrations.Count -gt 0
        executable_verified = $usable.Count -gt 0
        selected = if ($usable.Count -gt 0) { $usable[0] } else { $null }
        registrations = @($registrations)
    }
}

function Get-AiJarvisWindowPrimitiveProbe {
    if (-not ('AiJarvis.Setup.DwmApi' -as [type])) {
        Add-Type -TypeDefinition @'
using System.Runtime.InteropServices;
namespace AiJarvis.Setup {
    public static class DwmApi {
        [DllImport("dwmapi.dll", ExactSpelling = true)]
        public static extern int DwmIsCompositionEnabled([MarshalAs(UnmanagedType.Bool)] out bool enabled);
    }
}
'@
    }

    $compositionEnabled = $false
    $hresult = [AiJarvis.Setup.DwmApi]::DwmIsCompositionEnabled([ref]$compositionEnabled)
    $user32 = [IntPtr]::Zero
    $loaded = [Runtime.InteropServices.NativeLibrary]::TryLoad('user32.dll', [ref]$user32)
    $exports = [ordered]@{}
    try {
        $getLong = if ([IntPtr]::Size -eq 8) { 'GetWindowLongPtrW' } else { 'GetWindowLongW' }
        $setLong = if ([IntPtr]::Size -eq 8) { 'SetWindowLongPtrW' } else { 'SetWindowLongW' }
        foreach ($name in @($getLong, $setLong, 'SetLayeredWindowAttributes', 'SetWindowPos')) {
            $address = [IntPtr]::Zero
            $exports[$name] = $loaded -and [Runtime.InteropServices.NativeLibrary]::TryGetExport($user32, $name, [ref]$address)
        }
    }
    finally {
        if ($loaded) { [Runtime.InteropServices.NativeLibrary]::Free($user32) }
    }
    return [pscustomobject]@{
        dwm_call_succeeded = $hresult -ge 0
        dwm_hresult = '0x{0:X8}' -f ([int64]$hresult -band 0xFFFFFFFFL)
        dwm_composition_enabled = [bool]$compositionEnabled
        user32_loaded = [bool]$loaded
        user32_exports = [pscustomobject]$exports
    }
}

function Get-AiJarvisMicrophonePrivacyProbe {
    $consentRoot = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone'
    $policyPath = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\AppPrivacy'
    $readValue = {
        param($Path, $Name)
        try { return Get-ItemPropertyValue -LiteralPath $Path -Name $Name -ErrorAction Stop }
        catch { return $null }
    }

    return [pscustomobject]@{
        user_global = [string](& $readValue $consentRoot 'Value')
        user_non_packaged = [string](& $readValue (Join-Path $consentRoot 'NonPackaged') 'Value')
        machine_apps_policy = & $readValue $policyPath 'LetAppsAccessMicrophone'
    }
}

function ConvertTo-AiJarvisMicrophonePrivacyCheck {
    [CmdletBinding()]
    param([Parameter(Mandatory)] $Privacy)

    $possibleDenial = $Privacy.user_global -eq 'Deny' -or $Privacy.user_non_packaged -eq 'Deny'
    if ($possibleDenial) {
        return New-AiJarvisCheck microphone_privacy windows device warning 'Windows microphone registry values suggest desktop capture may be denied; final access is not proven.' 'Review Windows microphone privacy settings and verify the final executable during AG-06 smoke.' $Privacy
    }
    return New-AiJarvisCheck microphone_privacy windows device warning 'Windows microphone registry values contain no conclusive desktop-app denial; final access is not proven.' 'Verify microphone capture with the final desktop application during AG-06 smoke.' $Privacy
}

function Get-AiJarvisWindowsChecks {
    [CmdletBinding()]
    param()

    $checks = [Collections.Generic.List[object]]::new()
    if (-not $IsWindows) {
        $checks.Add((New-AiJarvisCheck windows_platform windows os_api fail 'AI Jarvis MVP requires Windows.' 'Run setup on Windows 10 1903 or later.'))
        return $checks.ToArray()
    }

    try {
        $build = [int](Get-ItemPropertyValue -LiteralPath 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' -Name CurrentBuildNumber -ErrorAction Stop)
        $status = if ($build -ge 18362) { 'pass' } else { 'fail' }
        $checks.Add((New-AiJarvisCheck windows_build windows os_api $status "Windows build $build" 'WGC monitor capture requires Windows 10 build 18362 or later.' @{ build = $build }))
    }
    catch {
        $checks.Add((New-AiJarvisCheck windows_build windows os_api error "Windows build probe failed: $($_.Exception.Message)" 'Inspect the Windows registry and setup probe.'))
    }

    try {
        $webView = Get-AiJarvisWebView2Probe
        if ($webView.executable_verified) {
            $checks.Add((New-AiJarvisCheck webview2 windows os_api pass "WebView2 $($webView.selected.version)" '' $webView))
        }
        elseif ($webView.registered) {
            $checks.Add((New-AiJarvisCheck webview2 windows os_api error 'WebView2 is registered, but its executable could not be verified.' 'Repair the WebView2 Evergreen Runtime installation.' $webView))
        }
        else {
            $checks.Add((New-AiJarvisCheck webview2 windows os_api fail 'WebView2 Evergreen Runtime is missing.' 'Install the Microsoft Edge WebView2 Evergreen Runtime.' $webView))
        }
    }
    catch {
        $checks.Add((New-AiJarvisCheck webview2 windows os_api error "WebView2 probe failed: $($_.Exception.Message)" 'Inspect the EdgeUpdate registration and setup probe.'))
    }

    try {
        $winRt = Get-AiJarvisWinRtProbe
        if ($null -ne $winRt.wgc.probeError) {
            $checks.Add((New-AiJarvisCheck wgc windows os_api error "WGC probe failed: $($winRt.wgc.probeError.message)" 'Inspect the WinRT projection and Windows installation.' $winRt.wgc))
        }
        elseif ($winRt.wgc.isSupported) {
            $checks.Add((New-AiJarvisCheck wgc windows os_api pass 'Windows Graphics Capture reports IsSupported=true.' '' $winRt.wgc))
        }
        else {
            $checks.Add((New-AiJarvisCheck wgc windows os_api fail 'Windows Graphics Capture is not supported.' 'Enable a supported Windows graphics environment.' $winRt.wgc))
        }

        if ($null -ne $winRt.audio.probeError) {
            $checks.Add((New-AiJarvisCheck audio_endpoints windows device error "Audio endpoint probe failed: $($winRt.audio.probeError.message)" 'Inspect Windows audio services and the WinRT probe.' $winRt.audio))
        }
        else {
            $renderPresent = -not [string]::IsNullOrWhiteSpace([string]$winRt.audio.defaultRenderId)
            $capturePresent = -not [string]::IsNullOrWhiteSpace([string]$winRt.audio.defaultCaptureId)
            $renderStatus = if ($renderPresent) { 'pass' } else { 'fail' }
            $captureStatus = if ($capturePresent) { 'pass' } else { 'fail' }
            $checks.Add((New-AiJarvisCheck wasapi_render_endpoint windows device $renderStatus 'Default audio render endpoint probe completed.' 'Connect and select a default audio output device.' @{ present = $renderPresent; id = $winRt.audio.defaultRenderId }))
            $checks.Add((New-AiJarvisCheck microphone_endpoint windows device $captureStatus 'Default microphone endpoint probe completed.' 'Connect and select a default microphone input device.' @{ present = $capturePresent; id = $winRt.audio.defaultCaptureId }))
        }
    }
    catch {
        $checks.Add((New-AiJarvisCheck winrt_probe windows os_api error "WinRT probe failed: $($_.Exception.Message)" 'Run Windows PowerShell 5.1 and inspect the WinRT projection.'))
    }

    $privacy = Get-AiJarvisMicrophonePrivacyProbe
    $checks.Add((ConvertTo-AiJarvisMicrophonePrivacyCheck $privacy))

    try {
        $windowProbe = Get-AiJarvisWindowPrimitiveProbe
        $allExports = @($windowProbe.user32_exports.psobject.Properties.Value) -notcontains $false
        if ($windowProbe.dwm_call_succeeded -and $windowProbe.dwm_composition_enabled -and $windowProbe.user32_loaded -and $allExports) {
            $checks.Add((New-AiJarvisCheck window_primitives windows os_api pass 'DWM composition and required User32 window primitives are available.' '' $windowProbe))
        }
        else {
            $checks.Add((New-AiJarvisCheck window_primitives windows os_api fail 'Transparent click-through window prerequisites are incomplete.' 'Enable desktop composition and repair the Windows desktop environment.' $windowProbe))
        }
    }
    catch {
        $checks.Add((New-AiJarvisCheck window_primitives windows os_api error "Window primitive probe failed: $($_.Exception.Message)" 'Inspect DWM/User32 and the setup probe.'))
    }

    foreach ($deferred in @(
        @{ Id = 'wgc_frame_smoke'; Message = 'A real WGC frame is deferred to AG-05.' },
        @{ Id = 'wasapi_loopback_smoke'; Message = 'A real WASAPI loopback frame is deferred to AG-06.' },
        @{ Id = 'microphone_capture_smoke'; Message = 'Desktop-app microphone capture is deferred to AG-06.' },
        @{ Id = 'click_through_smoke'; Message = 'Tauri hit-test and focus verification is deferred to AG-01/AG-07.' }
    )) {
        $checks.Add((New-AiJarvisCheck $deferred.Id runtime runtime_smoke deferred $deferred.Message))
    }
    return $checks.ToArray()
}

function Get-AiJarvisToolchainChecks {
    [CmdletBinding()]
    param([Parameter(Mandatory)][Collections.IDictionary] $Defaults)

    $nodeMajor = [int](Get-AiJarvisConfigValue $Defaults 'toolchain.node_min_major')
    $pnpmMajor = [int](Get-AiJarvisConfigValue $Defaults 'toolchain.pnpm_min_major')
    $rustMinimum = [version]([string](Get-AiJarvisConfigValue $Defaults 'toolchain.rust_min_version'))
    $checks = [Collections.Generic.List[object]]::new()

    $checks.Add((Test-AiJarvisTool node node @('--version') {
        param($text)
        if ($text -match 'v(?<version>[0-9]+\.[0-9]+\.[0-9]+)') { return [version]$Matches.version }
        return $null
    } { param($version) $version.Major -ge $nodeMajor } "Node.js major version $nodeMajor or later"))

    $checks.Add((Test-AiJarvisTool pnpm pnpm @('--version') {
        param($text)
        if ($text -match '(?<version>[0-9]+\.[0-9]+\.[0-9]+)') { return [version]$Matches.version }
        return $null
    } { param($version) $version.Major -ge $pnpmMajor } "pnpm major version $pnpmMajor or later"))

    $checks.Add((Test-AiJarvisTool rustc rustc @('--version') {
        param($text)
        if ($text -match 'rustc\s+(?<version>[0-9]+\.[0-9]+\.[0-9]+)') { return [version]$Matches.version }
        return $null
    } { param($version) $version -ge $rustMinimum } "Rust $rustMinimum or later"))

    $checks.Add((Test-AiJarvisTool cargo cargo @('--version') {
        param($text)
        if ($text -match 'cargo\s+(?<version>[0-9]+\.[0-9]+\.[0-9]+)') { return [version]$Matches.version }
        return $null
    } { param($version) $version -ge $rustMinimum } "Cargo $rustMinimum or later"))

    $rustc = Get-Command rustc -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -ne $rustc) {
        try {
            $details = Invoke-AiJarvisNativeCapture $rustc.Source @('-vV')
            if ($details.ExitCode -eq 0 -and $details.StdOut -match '(?m)^host:\s+x86_64-pc-windows-msvc\s*$') {
                $checks.Add((New-AiJarvisCheck rust_host toolchain static pass 'Rust host is x86_64-pc-windows-msvc.' '' @{ host = 'x86_64-pc-windows-msvc' }))
            }
            else {
                $checks.Add((New-AiJarvisCheck rust_host toolchain static fail 'Rust MSVC x86_64 host was not detected.' 'Install the x86_64-pc-windows-msvc Rust toolchain.' $details))
            }
        }
        catch {
            $checks.Add((New-AiJarvisCheck rust_host toolchain static error "Rust host probe failed: $($_.Exception.Message)" 'Inspect the Rust installation.'))
        }
    }
    return $checks.ToArray()
}

function Test-AiJarvisLlama {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][Collections.IDictionary] $Config,
        [string] $ExecutablePath
    )

    $path = Resolve-AiJarvisLlamaExecutable $Config $ExecutablePath
    if ([string]::IsNullOrWhiteSpace($path) -or -not (Test-Path -LiteralPath $path -PathType Leaf)) {
        return New-AiJarvisCheck llama model static fail 'Pinned llama.exe was not found.' "Install package '$([string](Get-AiJarvisConfigValue $Config 'llama.package_id'))' or set AI_JARVIS_LLAMA_EXE."
    }

    try {
        $item = Get-Item -LiteralPath $path -ErrorAction Stop
        $actualHash = (Get-FileHash -LiteralPath $path -Algorithm SHA256 -ErrorAction Stop).Hash.ToLowerInvariant()
        $expectedSize = [long](Get-AiJarvisConfigValue $Config 'llama.size_bytes')
        $expectedHash = ([string](Get-AiJarvisConfigValue $Config 'llama.sha256')).ToLowerInvariant()
        $versionOutput = Invoke-AiJarvisNativeCapture $path @('--version')
        $combined = ($versionOutput.StdOut + "`n" + $versionOutput.StdErr).Trim()
        $match = [regex]::Match($combined, 'version:\s*(?<version>[^\r\n(]+?)\s*\(build\s+(?<build>[0-9]+),\s*commit\s+(?<commit>[0-9a-f]+)\)', [Text.RegularExpressions.RegexOptions]::IgnoreCase)
        $expectedVersion = [string](Get-AiJarvisConfigValue $Config 'llama.version')
        $expectedBuild = [int](Get-AiJarvisConfigValue $Config 'llama.build')
        $expectedCommit = [string](Get-AiJarvisConfigValue $Config 'llama.commit')
        $versionMatches = $match.Success -and $match.Groups['version'].Value.Trim() -eq $expectedVersion
        $buildMatches = $match.Success -and [int]$match.Groups['build'].Value -eq $expectedBuild
        $reportedCommit = if ($match.Success) { $match.Groups['commit'].Value.ToLowerInvariant() } else { '' }
        $commitMatches = $reportedCommit.Length -ge 7 -and $expectedCommit.StartsWith($reportedCommit, [StringComparison]::OrdinalIgnoreCase)
        $integrityMatches = $item.Length -eq $expectedSize -and $actualHash -eq $expectedHash
        if ($versionOutput.TimedOut -or $versionOutput.ExitCode -ne 0 -or -not $versionMatches -or -not $buildMatches -or -not $commitMatches -or -not $integrityMatches) {
            return New-AiJarvisCheck llama model static fail 'llama.exe does not match the pinned build and file integrity contract.' 'Install the pinned ggml.llamacpp package build.' @{
                path = $path
                expected_version = $expectedVersion; expected_build = $expectedBuild; expected_commit = $expectedCommit
                expected_size = $expectedSize; expected_sha256 = $expectedHash
                actual_size = $item.Length; actual_sha256 = $actualHash; version_output = $combined
            }
        }
        return New-AiJarvisCheck llama model static pass "llama.cpp $expectedVersion build $expectedBuild ($reportedCommit)" '' @{ path = $path; size_bytes = $item.Length; sha256 = $actualHash; commit = $expectedCommit }
    }
    catch {
        return New-AiJarvisCheck llama model static error "llama.exe probe failed: $($_.Exception.Message)" 'Inspect the llama.cpp installation and setup probe.'
    }
}

function Test-AiJarvisTcpEndpoint {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string] $HostName,
        [ValidateRange(100, 10000)][int] $TimeoutMilliseconds = 3000
    )

    $client = [Net.Sockets.TcpClient]::new()
    try {
        $task = $client.ConnectAsync($HostName, 443)
        if (-not $task.Wait($TimeoutMilliseconds)) {
            return [pscustomobject]@{ reachable = $false; reason = 'timeout' }
        }
        if ($task.IsFaulted) {
            return [pscustomobject]@{ reachable = $false; reason = $task.Exception.GetBaseException().Message }
        }
        return [pscustomobject]@{ reachable = $client.Connected; reason = if ($client.Connected) { '' } else { 'connection failed' } }
    }
    catch {
        return [pscustomobject]@{ reachable = $false; reason = $_.Exception.Message }
    }
    finally {
        $client.Dispose()
    }
}

function Get-AiJarvisNetworkChecks {
    [CmdletBinding()]
    param([Parameter(Mandatory)][Collections.IDictionary] $Config)

    $checks = [Collections.Generic.List[object]]::new()
    foreach ($source in @(
        @{ Id = 'download_primary'; Host = 'hf-mirror.com'; Label = 'primary mirror' },
        @{ Id = 'download_fallback'; Host = 'huggingface.co'; Label = 'official fallback' }
    )) {
        $probe = Test-AiJarvisTcpEndpoint $source.Host
        if ($probe.reachable) {
            $checks.Add((New-AiJarvisCheck $source.Id network static pass "The $($source.Label) TCP endpoint is reachable." '' @{ host = $source.Host; port = 443 }))
        }
        else {
            $checks.Add((New-AiJarvisCheck $source.Id network static warning "The $($source.Label) TCP endpoint is not currently reachable: $($probe.reason)" 'Retry later or use already verified local artifacts.' @{ host = $source.Host; port = 443; reason = $probe.reason }))
        }
    }
    return $checks.ToArray()
}

function Get-AiJarvisExitCode {
    [CmdletBinding()]
    param([Parameter(Mandatory)][object[]] $Checks)

    if (@($Checks | Where-Object status -eq 'error').Count -gt 0) { return 2 }
    if (@($Checks | Where-Object status -eq 'fail').Count -gt 0) { return 1 }
    return 0
}

function Invoke-AiJarvisSetup {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][string] $ConfigPath,
        [Parameter(Mandatory)][string] $DefaultsPath,
        [string] $ModelRoot,
        [string] $LlamaExecutable,
        [scriptblock] $ToolchainProbe,
        [scriptblock] $WindowsProbe,
        [scriptblock] $NetworkProbe,
        [scriptblock] $LlamaProbe,
        [scriptblock] $ArtifactProbe,
        [scriptblock] $PreflightProbe
    )

    $checks = [Collections.Generic.List[object]]::new()
    try {
        $config = Read-AiJarvisToml $ConfigPath
        $defaults = Read-AiJarvisToml $DefaultsPath
        Assert-AiJarvisModelConfig $config
        Assert-AiJarvisDefaultsConfig $defaults
        $resolvedRoot = Resolve-AiJarvisModelRoot $config $ModelRoot
    }
    catch {
        $checks.Add((New-AiJarvisCheck configuration model static error "Configuration error: $($_.Exception.Message)" 'Repair resources/config/model.toml or defaults.toml.'))
        return [pscustomobject][ordered]@{ schema_version = 1; exit_code = 2; model_root = $null; checks = $checks.ToArray() }
    }

    foreach ($check in @($(if ($ToolchainProbe) { & $ToolchainProbe $config $defaults } else { Get-AiJarvisToolchainChecks $defaults }))) { $checks.Add($check) }
    foreach ($check in @($(if ($WindowsProbe) { & $WindowsProbe $config $defaults } else { Get-AiJarvisWindowsChecks }))) { $checks.Add($check) }
    foreach ($check in @($(if ($NetworkProbe) { & $NetworkProbe $config } else { Get-AiJarvisNetworkChecks $config }))) { $checks.Add($check) }

    $llamaCheck = if ($LlamaProbe) { & $LlamaProbe $config $LlamaExecutable } else { Test-AiJarvisLlama $config $LlamaExecutable }
    foreach ($check in @($llamaCheck)) { $checks.Add($check) }

    $artifactDefinitions = @(Get-AiJarvisArtifactDefinitions $config $resolvedRoot)
    foreach ($artifact in $artifactDefinitions) {
        $artifactCheck = if ($ArtifactProbe) { & $ArtifactProbe $artifact } else { Test-AiJarvisArtifact $artifact }
        foreach ($check in @($artifactCheck)) { $checks.Add($check) }
    }

    $qwenReady = @($checks | Where-Object { $_.id -in @('qwen_model', 'qwen_mmproj') -and $_.status -eq 'pass' }).Count -eq 2
    $llamaReady = @($checks | Where-Object { $_.id -eq 'llama' -and $_.status -eq 'pass' }).Count -eq 1
    if ($qwenReady -and $llamaReady) {
        try {
            $preflightResult = if ($PreflightProbe) {
                & $PreflightProbe $config $resolvedRoot $LlamaExecutable
            }
            else {
                Invoke-AiJarvisQwenPreflight $config $resolvedRoot $LlamaExecutable
            }
            foreach ($check in @($preflightResult)) { $checks.Add($check) }
        }
        catch {
            $checks.Add((New-AiJarvisCheck qwen_preflight model runtime_smoke error "Qwen preflight probe failed: $($_.Exception.Message)" 'Inspect the local model server and preflight probe.'))
        }
    }
    else {
        $checks.Add((New-AiJarvisCheck qwen_preflight model runtime_smoke deferred 'Qwen preflight was skipped because llama, GGUF, or mmproj did not pass integrity checks.' 'Install the pinned Qwen artifacts; the full real-model contract remains assigned to AG-03.'))
    }

    $resultChecks = $checks.ToArray()
    return [pscustomobject][ordered]@{
        schema_version = 1
        exit_code = Get-AiJarvisExitCode $resultChecks
        model_root = $resolvedRoot
        checks = $resultChecks
    }
}

function Get-AiJarvisEphemeralPort {
    $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 0)
    try {
        $listener.Start()
        return ([Net.IPEndPoint]$listener.LocalEndpoint).Port
    }
    finally {
        $listener.Stop()
    }
}

function Invoke-AiJarvisHttpJson {
    param(
        [Parameter(Mandatory)][Net.Http.HttpClient] $Client,
        [Parameter(Mandatory)][ValidateSet('GET', 'POST')][string] $Method,
        [Parameter(Mandatory)][Uri] $Uri,
        [object] $Body
    )

    $request = [Net.Http.HttpRequestMessage]::new([Net.Http.HttpMethod]::$Method, $Uri)
    try {
        if ($null -ne $Body) {
            $json = $Body | ConvertTo-Json -Depth 30 -Compress
            $request.Content = [Net.Http.StringContent]::new($json, [Text.Encoding]::UTF8, 'application/json')
        }
        $response = $Client.Send($request)
        try {
            $content = $response.Content.ReadAsStringAsync().GetAwaiter().GetResult()
            $parsed = if ([string]::IsNullOrWhiteSpace($content)) { $null } else { $content | ConvertFrom-Json -Depth 30 -ErrorAction Stop }
            return [pscustomobject]@{ status_code = [int]$response.StatusCode; success = $response.IsSuccessStatusCode; body = $parsed; raw = $content }
        }
        finally {
            $response.Dispose()
        }
    }
    finally {
        $request.Dispose()
    }
}

function Assert-AiJarvisNonThinkingResponse {
    param(
        [Parameter(Mandatory)] $Response,
        [Parameter(Mandatory)][string[]] $ExpectedProperties
    )

    if (-not $Response.success) { throw "Model request returned HTTP $($Response.status_code): $($Response.raw)" }
    if ($null -eq $Response.body.choices -or $Response.body.choices.Count -ne 1) { throw 'Model response did not contain exactly one choice.' }
    $message = $Response.body.choices[0].message
    if ($null -eq $message) { throw 'Model response did not contain a message.' }
    $content = [string]$message.content
    if ($content -match '(?is)<think>|</think>') { throw 'Model response contained a think block.' }
    $reasoningProperty = $message.PSObject.Properties['reasoning_content']
    if ($null -ne $reasoningProperty) {
        throw 'Model response contained reasoning_content.'
    }
    try {
        $parsed = $content | ConvertFrom-Json -Depth 10 -ErrorAction Stop
    }
    catch {
        throw "Model content was not strict JSON: $content"
    }
    if ($parsed -isnot [pscustomobject]) { throw 'Model content must be one JSON object.' }
    $actualProperties = @($parsed.PSObject.Properties.Name)
    if ($actualProperties.Count -ne $ExpectedProperties.Count) {
        throw "Model JSON properties did not match the strict schema: $($actualProperties -join ', ')."
    }
    foreach ($property in $ExpectedProperties) {
        if ($actualProperties -cnotcontains $property) { throw "Model JSON is missing strict property '$property'." }
    }
    return $parsed
}

function Invoke-AiJarvisQwenPreflight {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][Collections.IDictionary] $Config,
        [Parameter(Mandatory)][string] $ModelRoot,
        [string] $LlamaExecutable
    )

    $fixturePath = Join-Path $script:AiJarvisRepositoryRoot 'resources\prompts\probes\qwen-preflight.json'
    if (-not (Test-Path -LiteralPath $fixturePath -PathType Leaf)) {
        throw "Qwen preflight fixture is missing: $fixturePath"
    }
    $fixture = [IO.File]::ReadAllText($fixturePath, [Text.Encoding]::UTF8) | ConvertFrom-Json -Depth 30 -ErrorAction Stop
    if ($fixture.schema_version -ne 1) { throw 'Unsupported Qwen preflight fixture schema.' }

    $executable = Resolve-AiJarvisLlamaExecutable $Config $LlamaExecutable
    $port = Get-AiJarvisEphemeralPort
    $arguments = [Collections.Generic.List[string]]::new()
    foreach ($argument in @(Get-AiJarvisLlamaArguments $Config $ModelRoot -NoUi)) { $arguments.Add($argument) }
    $portIndex = $arguments.IndexOf('--port')
    if ($portIndex -lt 0 -or $portIndex + 1 -ge $arguments.Count) { throw 'Generated llama arguments do not contain a port value.' }
    $arguments[$portIndex + 1] = [string]$port

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $executable
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    foreach ($argument in $arguments) { [void]$startInfo.ArgumentList.Add($argument) }

    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $client = [Net.Http.HttpClient]::new()
    $client.Timeout = [TimeSpan]::FromSeconds(15)
    $stdoutTask = $null
    $stderrTask = $null
    try {
        if (-not $process.Start()) { throw 'Failed to start llama for Qwen preflight.' }
        $stdoutTask = $process.StandardOutput.ReadToEndAsync()
        $stderrTask = $process.StandardError.ReadToEndAsync()
        $baseUri = "http://127.0.0.1:$port"
        $deadline = [DateTime]::UtcNow.AddSeconds([int]$fixture.startup_timeout_seconds)
        $ready = $false
        while ([DateTime]::UtcNow -lt $deadline) {
            if ($process.HasExited) { throw "llama exited during startup with code $($process.ExitCode)." }
            try {
                $health = Invoke-AiJarvisHttpJson $client GET ([Uri]"$baseUri/health")
                if ($health.status_code -eq 200 -and $health.body.status -eq 'ok') {
                    $ready = $true
                    break
                }
                if ($health.status_code -ne 503) {
                    throw "Unexpected health response HTTP $($health.status_code): $($health.raw)"
                }
            }
            catch [Net.Http.HttpRequestException] {}
            catch [Threading.Tasks.TaskCanceledException] {}
            Start-Sleep -Milliseconds 750
        }
        if (-not $ready) { throw 'llama did not become ready before the preflight deadline.' }

        $models = Invoke-AiJarvisHttpJson $client GET ([Uri]"$baseUri/v1/models")
        $expectedAlias = [string](Get-AiJarvisConfigValue $Config 'server.alias')
        if (-not $models.success -or @($models.body.data | Where-Object id -eq $expectedAlias).Count -ne 1) {
            throw "The pinned model alias '$expectedAlias' was not exposed by /v1/models."
        }

        $textResponseFormat = [ordered]@{
            type = 'json_schema'
            json_schema = [ordered]@{ name = 'ai_jarvis_text_preflight'; strict = $true; schema = $fixture.text_response_schema }
        }
        $visionResponseFormat = [ordered]@{
            type = 'json_schema'
            json_schema = [ordered]@{ name = 'ai_jarvis_vision_preflight'; strict = $true; schema = $fixture.vision_response_schema }
        }
        $sampling = [ordered]@{
            temperature = [double](Get-AiJarvisConfigValue $Config 'qwen.sampling.temperature')
            top_p = [double](Get-AiJarvisConfigValue $Config 'qwen.sampling.top_p')
            top_k = [int](Get-AiJarvisConfigValue $Config 'qwen.sampling.top_k')
            min_p = [double](Get-AiJarvisConfigValue $Config 'qwen.sampling.min_p')
            presence_penalty = [double](Get-AiJarvisConfigValue $Config 'qwen.sampling.presence_penalty')
            repeat_penalty = [double](Get-AiJarvisConfigValue $Config 'qwen.sampling.repeat_penalty')
        }
        $textRequest = [ordered]@{
            model = $expectedAlias
            messages = @([ordered]@{ role = 'user'; content = [string]$fixture.text_prompt })
            response_format = $textResponseFormat
            max_tokens = 64
        }
        foreach ($key in $sampling.Keys) { $textRequest[$key] = $sampling[$key] }
        $textResponse = Invoke-AiJarvisHttpJson $client POST ([Uri]"$baseUri/v1/chat/completions") $textRequest
        $textContent = Assert-AiJarvisNonThinkingResponse $textResponse @('status')
        if ($textContent.status -ne 'ok') { throw 'Text preflight JSON did not satisfy the expected value.' }

        $visionContent = [Collections.Generic.List[object]]::new()
        $visionContent.Add([ordered]@{ type = 'text'; text = [string]$fixture.vision_prompt })
        foreach ($relativeImage in $fixture.image_paths) {
            $imagePath = [IO.Path]::GetFullPath((Join-Path $script:AiJarvisRepositoryRoot ([string]$relativeImage)))
            if (-not (Test-Path -LiteralPath $imagePath -PathType Leaf)) { throw "Preflight image is missing: $imagePath" }
            $bytes = [IO.File]::ReadAllBytes($imagePath)
            $extension = [IO.Path]::GetExtension($imagePath).ToLowerInvariant()
            $mime = if ($extension -eq '.png') { 'image/png' } elseif ($extension -in @('.jpg', '.jpeg')) { 'image/jpeg' } else { throw "Unsupported preflight image type: $extension" }
            $visionContent.Add([ordered]@{ type = 'image_url'; image_url = [ordered]@{ url = "data:$mime;base64,$([Convert]::ToBase64String($bytes))" } })
        }
        $visionRequest = [ordered]@{
            model = $expectedAlias
            messages = @([ordered]@{ role = 'user'; content = $visionContent.ToArray() })
            response_format = $visionResponseFormat
            max_tokens = 64
        }
        foreach ($key in $sampling.Keys) { $visionRequest[$key] = $sampling[$key] }
        $visionResponse = Invoke-AiJarvisHttpJson $client POST ([Uri]"$baseUri/v1/chat/completions") $visionRequest
        $visionResult = Assert-AiJarvisNonThinkingResponse $visionResponse @('status', 'poses')
        $expectedPoses = @($fixture.expected_poses)
        $actualPoses = @($visionResult.poses)
        if ($visionResult.status -ne 'ok' -or $actualPoses.Count -ne $expectedPoses.Count) {
            throw 'Multi-image preflight JSON did not satisfy the expected shape.'
        }
        for ($index = 0; $index -lt $expectedPoses.Count; $index++) {
            if ([string]$actualPoses[$index] -cne [string]$expectedPoses[$index]) {
                throw "Multi-image order/content mismatch at index $index."
            }
        }

        return New-AiJarvisCheck qwen_preflight model runtime_smoke pass 'Qwen text, two-image, strict JSON, model alias, and non-thinking preflight passed.' '' @{ image_count = $fixture.image_paths.Count; port = $port }
    }
    catch {
        return New-AiJarvisCheck qwen_preflight model runtime_smoke fail "Qwen preflight failed: $($_.Exception.Message)" 'Inspect llama logs and keep the AG-03 real-model gate open.' @{ port = $port }
    }
    finally {
        $client.Dispose()
        if (-not $process.HasExited) {
            try { $process.Kill($true); $process.WaitForExit(10000) | Out-Null } catch {}
        }
        if ($null -ne $stdoutTask) { try { [void]$stdoutTask.GetAwaiter().GetResult() } catch {} }
        if ($null -ne $stderrTask) { try { [void]$stderrTask.GetAwaiter().GetResult() } catch {} }
        $process.Dispose()
    }
}

Export-ModuleMember -Function @(
    'ConvertFrom-AiJarvisToml',
    'Read-AiJarvisToml',
    'Get-AiJarvisConfigValue',
    'Assert-AiJarvisModelConfig',
    'Assert-AiJarvisDefaultsConfig',
    'Resolve-AiJarvisModelRoot',
    'Get-AiJarvisArtifactDefinitions',
    'Resolve-AiJarvisLlamaExecutable',
    'Get-AiJarvisLlamaArguments',
    'New-AiJarvisCheck',
    'Invoke-AiJarvisNativeCapture',
    'Invoke-AiJarvisWindowsPowerShell51',
    'Get-AiJarvisWinRtProbe',
    'Get-AiJarvisWebView2Probe',
    'Get-AiJarvisMicrophonePrivacyProbe',
    'ConvertTo-AiJarvisMicrophonePrivacyCheck',
    'Get-AiJarvisWindowPrimitiveProbe',
    'Get-AiJarvisWindowsChecks',
    'Get-AiJarvisToolchainChecks',
    'Test-AiJarvisLlama',
    'Test-AiJarvisTcpEndpoint',
    'Get-AiJarvisNetworkChecks',
    'Get-AiJarvisExitCode',
    'Invoke-AiJarvisSetup',
    'Invoke-AiJarvisQwenPreflight',
    'Assert-AiJarvisNonThinkingResponse',
    'Test-AiJarvisTool',
    'Test-AiJarvisArtifact'
)
