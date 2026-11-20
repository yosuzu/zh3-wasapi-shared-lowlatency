# pwsh -File tools/build-community.ps1 -BuildDir target/community
# pwsh -File tools/build-community.ps1 -Mode Test -BuildDir target/community
# Pass -TestName with the full Rust test name. Add -Ignored for opt-in tests.
[CmdletBinding()]
param(
    [ValidateSet('Build', 'Test')] [string] $Mode = 'Build',
    [string] $BuildDir,
    [string] $TestName,
    [switch] $Ignored
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$target = 'x86_64-pc-windows-msvc'
if (!$BuildDir) { $BuildDir = Join-Path $root ("target/community-" + [guid]::NewGuid().ToString('N')) }
$BuildDir = [IO.Path]::GetFullPath($BuildDir)
if (($TestName -or $Ignored) -and $Mode -ne 'Test') { throw 'TestName and Ignored require -Mode Test.' }
if ($Ignored -and !$TestName) { throw 'Specify one exact test name with -Ignored.' }
if ($TestName -like '-*') { throw 'Specify a test name without command-line options.' }

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio/Installer/vswhere.exe'
if (!(Test-Path $vswhere)) { throw 'Install Visual Studio Community 2026 and Desktop development with C++.' }
$vs = & $vswhere -latest -products Microsoft.VisualStudio.Product.Community -version '[18.0,19.0)' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -format json | ConvertFrom-Json
if (!$vs -or $vs.isPrerelease -or !$vs.isComplete) { throw 'Stable Visual Studio Community 2026 with x64 C++ tools was not found.' }

# cmd /u preserves Unicode installation paths while importing VsDevCmd's environment.
$start = New-Object Diagnostics.ProcessStartInfo
$start.FileName = $env:ComSpec
$start.Arguments = '/d /u /c "call "' + $vs.installationPath + '\Common7\Tools\VsDevCmd.bat" -no_logo -arch=x64 -host_arch=x64 >nul && set"'
$start.UseShellExecute = $false
$start.RedirectStandardOutput = $true
$start.StandardOutputEncoding = [Text.Encoding]::Unicode
$process = [Diagnostics.Process]::Start($start)
$devEnv = $process.StandardOutput.ReadToEnd()
$process.WaitForExit()
if ($process.ExitCode) { throw 'VsDevCmd could not initialize the x64 environment.' }
foreach ($line in $devEnv -split '\r?\n') {
    if ($line -match '^([^=]+)=(.*)$') { [Environment]::SetEnvironmentVariable($Matches[1], $Matches[2], 'Process') }
}
$vcBin = Join-Path $env:VCToolsInstallDir 'bin/Hostx64/x64'
$sdkBin = Join-Path $env:WindowsSdkVerBinPath 'x64'
$env:Path = "$vcBin;$sdkBin;" + $env:Path
if (!$env:VCToolsInstallDir.StartsWith(($vs.installationPath.TrimEnd('\') + '\VC\Tools\MSVC\'), [StringComparison]::OrdinalIgnoreCase)) {
    throw 'VsDevCmd selected a toolset outside the selected Community installation.'
}
foreach ($tool in @('link.exe','cl.exe','lib.exe','rc.exe','mt.exe')) {
    $bin = if ($tool -in 'rc.exe','mt.exe') { $sdkBin } else { $vcBin }
    $expected = (Get-Item -LiteralPath (Join-Path $bin $tool)).FullName
    if ((Get-Command $tool -CommandType Application | Select-Object -First 1).Source -ne $expected) { throw "Unexpected $tool on PATH." }
}
$linker = (Get-Item (Join-Path $vcBin 'link.exe')).FullName
$env:CC_x86_64_pc_windows_msvc = Join-Path $vcBin 'cl.exe'
$env:CXX_x86_64_pc_windows_msvc = $env:CC_x86_64_pc_windows_msvc
$env:AR_x86_64_pc_windows_msvc = Join-Path $vcBin 'lib.exe'
$env:RC_PATH = Join-Path $sdkBin 'rc.exe'
$env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER = $linker

$cache = Join-Path $root 'tmp/rust-tools'
if (!$env:CARGO_HOME -and !(Get-Command cargo.exe -ErrorAction SilentlyContinue) -and (Test-Path "$cache/cargo/bin/cargo.exe")) {
    $env:CARGO_HOME = Join-Path $cache 'cargo'
    if (!$env:RUSTUP_HOME) { $env:RUSTUP_HOME = Join-Path $cache 'rustup' }
}
if ($env:CARGO_HOME -and (Test-Path "$env:CARGO_HOME/bin/cargo.exe")) {
    $env:Path = (Join-Path $env:CARGO_HOME 'bin') + ';' + $env:Path
}
if (!(Get-Command rustc.exe -ErrorAction SilentlyContinue) -or !(Get-Command cargo.exe -ErrorAction SilentlyContinue)) {
    throw 'Install Rust stable x64 MSVC with Cargo, or configure CARGO_HOME and RUSTUP_HOME.'
}
$rustInfo = & rustc.exe -vV
if ($LASTEXITCODE -or $rustInfo -notcontains "host: $target") { throw "Use a Rust toolchain hosted on $target." }
$sysroot = & rustc.exe --print sysroot
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$flags = @('-Ctarget-feature=+crt-static', "-Clinker=$linker", '-Clink-arg=/PDBALTPATH:%_PDB%')
foreach ($mapping in @(@($root,'/source'), @($BuildDir,'/build'), @($cargoHome,'/cargo'), @($sysroot,'/rust'))) {
    $flags += '--remap-path-prefix=' + [IO.Path]::GetFullPath($mapping[0]) + '=' + $mapping[1]
}
$env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]31
$env:CARGO_TARGET_DIR = Join-Path $BuildDir 'target'
$null = New-Item -ItemType Directory -Force -Path $BuildDir
$manifest = Join-Path $root 'app/Cargo.toml'

# Native mode applies the same flags to host build scripts, proc macros and the app.
# Run outside app/.cargo, which normally sets an explicit target for cross compilation.
Push-Location -LiteralPath $root
try {
    if ($Mode -eq 'Test') {
        $testArgs = @('test', '--release', '--locked', '--manifest-path', $manifest)
        if ($TestName) { $testArgs += @($TestName, '--', '--exact', '--nocapture') }
        if ($Ignored) { $testArgs += '--ignored' }
        $ranTest = $false
        & cargo.exe @testArgs | ForEach-Object {
            if ($_ -match '^test result: ok\. [1-9]\d* passed;') { $ranTest = $true }
            Write-Output $_
        }
        if ($LASTEXITCODE) { throw "Cargo test failed ($LASTEXITCODE)." }
        if ($TestName -and !$ranTest) { throw "No test passed with the exact name '$TestName'." }
        return
    }
    $observedLink = $false
    & cargo.exe rustc --locked --release --manifest-path $manifest --bin ZH3-Control -- --print=link-args | ForEach-Object {
        if ($_.Replace('\\','\').StartsWith(('"' + $linker + '" '), [StringComparison]::OrdinalIgnoreCase) -and
            $_ -match '(?i)/OUT:.*ZH3.Control[^"]*\.exe"') { $observedLink = $true }
    }
    if ($LASTEXITCODE) { throw "Cargo build failed ($LASTEXITCODE)." }
    $exe = Get-Item (Join-Path $env:CARGO_TARGET_DIR 'release/ZH3-Control.exe')
    $sha = (Get-FileHash $exe.FullName -Algorithm SHA256).Hash
    $report = Join-Path $BuildDir 'build.json'
    if (!$observedLink) {
        if (!(Test-Path $report) -or (Get-Content -Raw $report | ConvertFrom-Json).sha256 -ne $sha) {
            throw 'No matching final linker invocation was observed. Retry with a fresh -BuildDir.'
        }
    } else {
        [ordered]@{
            product = $vs.displayName; visualStudio = $vs.installationVersion; msvc = $env:VCToolsVersion
            linker = (Get-Item $linker).VersionInfo.FileVersion; linkerSha256 = (Get-FileHash $linker).Hash
            compiler = (Get-Item $env:CC_x86_64_pc_windows_msvc).VersionInfo.FileVersion
            resourceCompiler = (Get-Item $env:RC_PATH).VersionInfo.FileVersion
            sdk = $env:WindowsSDKVersion.TrimEnd('\'); rustc = $rustInfo[0]; cargo = (& cargo.exe -V)
            target = $target; staticCrt = $true; finalLinkCommandObserved = $true
            fileVersion = $exe.VersionInfo.FileVersion; bytes = $exe.Length; sha256 = $sha
        } | ConvertTo-Json | Set-Content -Encoding UTF8 -LiteralPath $report
    }
    # Cargo can replace its top-level executable during later integration tests.
    $candidate = Join-Path $BuildDir 'ZH3-Control.exe'
    if (!(Test-Path -LiteralPath $candidate) -or (Get-FileHash -LiteralPath $candidate).Hash -ne $sha) {
        Copy-Item -LiteralPath $exe.FullName -Destination $candidate
    }
    Write-Output $candidate
    Write-Output $report
} finally { Pop-Location }
