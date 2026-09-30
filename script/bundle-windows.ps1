[CmdletBinding()]
Param(
    [Parameter()][Alias('i')][switch]$Install,
    [Parameter()][Alias('h')][switch]$Help,
    [Parameter()][Alias('a')][string]$Architecture,
    [Parameter()][string]$Name,
    # Also build the (optional) remote_server zip. Off by default to keep builds light.
    [Parameter()][switch]$RemoteServer
)

# OTerminal Windows bundle script (based on Zed's bundle-windows.ps1).
# Produces target\OTerminal-<arch>.exe (Inno Setup installer) containing:
#   oterminal.exe, conpty.dll, OpenConsole.exe (next to the exe and in x64\ / arm64\),
#   bin\oterminal.exe (CLI), bin\oterminal (WSL shim), tools\auto_update_helper.exe

. "$PSScriptRoot/lib/workspace.ps1"

# https://stackoverflow.com/questions/57949031/powershell-script-stops-if-program-fails-like-bash-set-o-errexit
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$buildSuccess = $false
$canCodeSign = $false

$OSArchitecture = switch ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture) {
    "X64" { "x86_64" }
    "Arm64" { "aarch64" }
    default { throw "Unsupported architecture" }
}

$Architecture = if ($Architecture) {
    $Architecture
} else {
    $OSArchitecture
}

# Honour CARGO_TARGET_DIR (CI keeps the target dir on the drive with the most free space).
$CargoTargetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { "./target" }
$CargoOutDir = "$CargoTargetDir/$Architecture-pc-windows-msvc/release"

# Keep in sync with the ConPTY version downloaded by crates/zed/build.rs.
$ConptyVersionTag = "v1.24.10621.0"
$ConptyPackage = "Microsoft.Windows.Console.ConPTY.1.24.260303001.nupkg"

function Get-VSArch {
    param(
        [string]$Arch
    )

    switch ($Arch) {
        "x86_64" { "amd64" }
        "aarch64" { "arm64" }
    }
}

if ($Help) {
    Write-Output "Usage: bundle-windows.ps1 [-Install] [-RemoteServer] [-Help]"
    Write-Output "Build the OTerminal installer for Windows.`n"
    Write-Output "Options:"
    Write-Output "  -Architecture, -a Which architecture to build (x86_64 or aarch64)"
    Write-Output "  -Install, -i      Run the installer after building."
    Write-Output "  -RemoteServer     Also build and zip remote_server."
    Write-Output "  -Help, -h         Show this help message."
    exit 0
}

$vsDevShell = Get-ChildItem -Path "C:\Program Files\Microsoft Visual Studio\*\*\Common7\Tools\Launch-VsDevShell.ps1", "C:\Program Files (x86)\Microsoft Visual Studio\*\*\Common7\Tools\Launch-VsDevShell.ps1" -ErrorAction SilentlyContinue | Select-Object -First 1
if ($vsDevShell) {
    Push-Location
    & $vsDevShell.FullName -Arch (Get-VSArch -Arch $Architecture) -HostArch (Get-VSArch -Arch $OSArchitecture)
    Pop-Location
} else {
    Write-Output "Visual Studio developer shell not found; assuming the MSVC toolchain is already on PATH."
}

$target = "$Architecture-pc-windows-msvc"

Push-Location -Path crates/zed
$channel = (Get-Content "RELEASE_CHANNEL").Trim()
$env:ZED_RELEASE_CHANNEL = $channel
$env:RELEASE_CHANNEL = $channel
Pop-Location

function CheckEnvironmentVariables {
    if(-not $env:CI) {
        return
    }

    $requiredVars = @('ZED_WORKSPACE', 'RELEASE_VERSION', 'ZED_RELEASE_CHANNEL')

    foreach ($var in $requiredVars) {
        if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($var))) {
            Write-Error "$var is not set"
            exit 1
        }
    }

    # When the signing secrets are not populated, skip code signing instead of failing.
    $signingVars = @(
        'AZURE_TENANT_ID', 'AZURE_CLIENT_ID', 'AZURE_CLIENT_SECRET',
        'ACCOUNT_NAME', 'CERT_PROFILE_NAME', 'ENDPOINT',
        'FILE_DIGEST', 'TIMESTAMP_DIGEST', 'TIMESTAMP_SERVER'
    )

    $missingVars = @($signingVars | Where-Object { [string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($_)) })
    if ($missingVars.Count -eq 0) {
        $script:canCodeSign = $true
    } else {
        Write-Host "====== WARNING ======"
        Write-Host "One or more of the following variables are missing: $($missingVars -join ', ')"
        Write-Host "This bundle will not be code signed"
        Write-Host "====== WARNING ======"
    }
}

function PrepareForBundle {
    if (Test-Path "$innoDir") {
        Remove-Item -Path "$innoDir" -Recurse -Force
    }
    New-Item -Path "$innoDir" -ItemType Directory -Force
    Copy-Item -Path "$env:ZED_WORKSPACE\crates\zed\resources\windows\*" -Destination "$innoDir" -Recurse -Force
    # The Windows 11 explorer command injector (appx) is Zed-signed and not built for OTerminal;
    # zed.iss falls back to the classic registry context menu when appx\ is empty.
    New-Item -Path "$innoDir\appx" -ItemType Directory -Force
    New-Item -Path "$innoDir\bin" -ItemType Directory -Force
    New-Item -Path "$innoDir\tools" -ItemType Directory -Force

    rustup target add $target
}

function GenerateLicenses {
    . $PSScriptRoot/generate-licenses.ps1
}

function BuildOTerminalAndItsFriends {
    Write-Output "Building OTerminal and its friends, for channel: $channel"
    # Build oterminal.exe (package zed), cli.exe and auto_update_helper.exe
    cargo --config .cargo/bundle-config.toml build --release --package zed --package cli --package auto_update_helper --target $target

    # The zed package's [[bin]] is named "oterminal"; fall back to zed.exe for older trees.
    $appExe = "$CargoOutDir\oterminal.exe"
    if (-not (Test-Path $appExe)) {
        $appExe = "$CargoOutDir\zed.exe"
    }
    Copy-Item -Path $appExe -Destination "$innoDir\oterminal.exe" -Force
    Copy-Item -Path "$CargoOutDir\cli.exe" -Destination "$innoDir\cli.exe" -Force
    Copy-Item -Path "$CargoOutDir\auto_update_helper.exe" -Destination "$innoDir\auto_update_helper.exe" -Force
}

function BuildRemoteServer {
    Write-Output "Building remote_server for $target"
    cargo --config .cargo/bundle-config.toml build --release --package remote_server --target $target

    # Create zipped remote server binary
    $remoteServerSrc = (Resolve-Path "$CargoOutDir\remote_server.exe").Path

    if ($canCodeSign) {
        Write-Output "Code signing remote_server.exe"
        & "$innoDir\sign.ps1" $remoteServerSrc
    }

    $remoteServerDst = "$env:ZED_WORKSPACE\target\oterminal-remote-server-windows-$Architecture.zip"
    Write-Output "Compressing remote_server to $remoteServerDst"
    Compress-Archive -Path $remoteServerSrc -DestinationPath $remoteServerDst -Force

    Write-Output "Remote server compressed successfully"
}

function ZipOTerminalDebug {
    $items = @(
        "$CargoOutDir\oterminal.pdb",
        "$CargoOutDir\zed.pdb",
        "$CargoOutDir\cli.pdb",
        "$CargoOutDir\auto_update_helper.pdb",
        "$CargoOutDir\remote_server.pdb"
    ) | Where-Object { Test-Path $_ }

    if ($items.Count -gt 0) {
        Compress-Archive -Path $items -DestinationPath $debugArchive -Force
    }
}

function SignOTerminalAndItsFriends {
    if (-not $canCodeSign) {
        return
    }

    $files = "$innoDir\oterminal.exe,$innoDir\cli.exe,$innoDir\auto_update_helper.exe"
    & "$innoDir\sign.ps1" $files
}

function DownloadAMDGpuServices {
    # If you update the AGS SDK version, please also update the version in `crates/gpui/src/platform/windows/directx_renderer.rs`
    $url = "https://codeload.github.com/GPUOpen-LibrariesAndSDKs/AGS_SDK/zip/refs/tags/v6.3.0"
    $zipPath = ".\AGS_SDK_v6.3.0.zip"
    # Download the AGS SDK zip file
    Invoke-WebRequest -Uri $url -OutFile $zipPath
    # Extract the AGS SDK zip file
    Expand-Archive -Path $zipPath -DestinationPath "." -Force
}

function DownloadConpty {
    # ConPTY (conpty.dll + OpenConsole.exe) is required for correct mouse and key
    # handling in terminal TUIs such as Claude Code.
    $url = "https://github.com/microsoft/terminal/releases/download/$ConptyVersionTag/$ConptyPackage"
    $zipPath = ".\$ConptyPackage.zip"
    Invoke-WebRequest -Uri $url -OutFile $zipPath
    if (Test-Path ".\conpty") {
        Remove-Item -Path ".\conpty" -Recurse -Force
    }
    Expand-Archive -Path $zipPath -DestinationPath ".\conpty" -Force
}

function CollectFiles {
    Move-Item -Path "$innoDir\cli.exe" -Destination "$innoDir\bin\oterminal.exe" -Force
    Move-Item -Path "$innoDir\zed.sh" -Destination "$innoDir\bin\oterminal" -Force
    Move-Item -Path "$innoDir\auto_update_helper.exe" -Destination "$innoDir\tools\auto_update_helper.exe" -Force
    New-Item -Type Directory -Path "$innoDir\arm64" -Force
    Copy-Item -Path ".\conpty\build\native\runtimes\arm64\OpenConsole.exe" -Destination "$innoDir\arm64\OpenConsole.exe" -Force
    if($Architecture -eq "aarch64") {
        # conpty.dll and OpenConsole.exe next to oterminal.exe (same layout as `cargo build`).
        Copy-Item -Path ".\conpty\build\native\runtimes\arm64\OpenConsole.exe" -Destination "$innoDir\OpenConsole.exe" -Force
        Copy-Item -Path ".\conpty\runtimes\win-arm64\native\conpty.dll" -Destination "$innoDir\conpty.dll" -Force
    }
    else {
        New-Item -Type Directory -Path "$innoDir\x64" -Force
        Move-Item -Path ".\AGS_SDK-6.3.0\ags_lib\lib\amd_ags_x64.dll" -Destination "$innoDir\amd_ags_x64.dll" -Force
        Copy-Item -Path ".\conpty\build\native\runtimes\x64\OpenConsole.exe" -Destination "$innoDir\x64\OpenConsole.exe" -Force
        # conpty.dll and OpenConsole.exe next to oterminal.exe (same layout as `cargo build`).
        Copy-Item -Path ".\conpty\build\native\runtimes\x64\OpenConsole.exe" -Destination "$innoDir\OpenConsole.exe" -Force
        Copy-Item -Path ".\conpty\runtimes\win-x64\native\conpty.dll" -Destination "$innoDir\conpty.dll" -Force
    }
}

function BuildInstaller {
    $issFilePath = "$innoDir\zed.iss"
    # The mutex names below must match release_channel::app_identifier() + "-Instance-Mutex"
    # (see crates\zed\src\zed\windows_only_instance.rs).
    switch ($channel) {
        "stable" {
            $appId = "{{6F1B3C2A-5E4D-4A7B-9C1E-0A7E5D1B2C01}"
            $appIconName = "app-icon"
            $appName = "OTerminal"
            $appDisplayName = "OTerminal"
            $appMutex = "OTerminal-Stable-Instance-Mutex"
            $regValueName = "OTerminal"
            $appUserId = "com.othcloud.oterminal"
            $appShellNameShort = "O&Terminal"
        }
        "preview" {
            $appId = "{{6F1B3C2A-5E4D-4A7B-9C1E-0A7E5D1B2C02}"
            $appIconName = "app-icon-preview"
            $appName = "OTerminal Preview"
            $appDisplayName = "OTerminal Preview"
            $appMutex = "OTerminal-Preview-Instance-Mutex"
            $regValueName = "OTerminalPreview"
            $appUserId = "com.othcloud.oterminal.preview"
            $appShellNameShort = "O&Terminal Preview"
        }
        "nightly" {
            $appId = "{{6F1B3C2A-5E4D-4A7B-9C1E-0A7E5D1B2C03}"
            $appIconName = "app-icon-nightly"
            $appName = "OTerminal Nightly"
            $appDisplayName = "OTerminal Nightly"
            $appMutex = "OTerminal-Nightly-Instance-Mutex"
            $regValueName = "OTerminalNightly"
            $appUserId = "com.othcloud.oterminal.nightly"
            $appShellNameShort = "O&Terminal Nightly"
        }
        "dev" {
            $appId = "{{6F1B3C2A-5E4D-4A7B-9C1E-0A7E5D1B2C04}"
            $appIconName = "app-icon-dev"
            $appName = "OTerminal Dev"
            $appDisplayName = "OTerminal Dev"
            $appMutex = "OTerminal-Dev-Instance-Mutex"
            $regValueName = "OTerminalDev"
            $appUserId = "com.othcloud.oterminal.dev"
            $appShellNameShort = "O&Terminal Dev"
        }
        default {
            Write-Error "can't bundle installer for $channel."
            exit 1
        }
    }
    $appSetupName = "OTerminal-$Architecture"
    $appExeName = "oterminal"
    # Only used when an explorer command injector appx is bundled (not by default).
    $appAppxFullName = "OverTimeHosting.OTerminal_1.0.0.0_neutral__0000000000000"

    # Windows runner 2022 default has iscc in PATH, https://github.com/actions/runner-images/blob/main/images/windows/Windows2022-Readme.md
    # Windows runner 2025 doesn't have iscc in PATH for now, https://github.com/actions/runner-images/issues/11228
    $innoSetupPath = "C:\Program Files (x86)\Inno Setup 6\ISCC.exe"
    if ($env:ISCC_PATH) {
        $innoSetupPath = $env:ISCC_PATH
    } elseif (-not (Test-Path $innoSetupPath)) {
        $iscc = Get-Command iscc.exe -ErrorAction SilentlyContinue
        if ($iscc) {
            $innoSetupPath = $iscc.Source
        } elseif (Test-Path "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe") {
            $innoSetupPath = "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
        }
    }

    $definitions = @{
        "AppId"          = $appId
        "AppIconName"    = $appIconName
        "OutputDir"      = "$env:ZED_WORKSPACE\target"
        "AppSetupName"   = $appSetupName
        "AppName"        = $appName
        "AppDisplayName" = $appDisplayName
        "RegValueName"   = $regValueName
        "AppMutex"       = $appMutex
        "AppExeName"     = $appExeName
        "ResourcesDir"   = "$innoDir"
        "ShellNameShort" = $appShellNameShort
        "AppUserId"      = $appUserId
        "Version"        = "$env:RELEASE_VERSION"
        "SourceDir"      = "$env:ZED_WORKSPACE"
        "AppxFullName"   = $appAppxFullName
    }

    $defs = @()
    foreach ($key in $definitions.Keys) {
        $defs += "/d$key=`"$($definitions[$key])`""
    }

    $innoArgs = @($issFilePath) + $defs
    if($canCodeSign) {
        # Checked by zed.iss to decide whether to sign the installer.
        $env:ZED_SIGN_BUNDLE = "1"
        $signTool = "powershell.exe -ExecutionPolicy Bypass -File $innoDir\sign.ps1 `$f"
        $innoArgs += "/sDefaultsign=`"$signTool`""
    }

    # Execute Inno Setup
    Write-Host "Running Inno Setup: $innoSetupPath $innoArgs"
    $process = Start-Process -FilePath $innoSetupPath -ArgumentList $innoArgs -NoNewWindow -Wait -PassThru

    if ($process.ExitCode -eq 0) {
        Write-Host "Inno Setup successfully compiled the installer"
        if ($env:GITHUB_ENV) {
            Write-Output "SETUP_PATH=target/$appSetupName.exe" >> $env:GITHUB_ENV
        }
        $script:setupPath = "$env:ZED_WORKSPACE\target\$appSetupName.exe"
        $script:buildSuccess = $true
    }
    else {
        Write-Host "Inno Setup failed: $($process.ExitCode)"
        $script:buildSuccess = $false
    }
}

ParseZedWorkspace
# ParseZedWorkspace sets RELEASE_VERSION to the zed crate version (the Zed
# release OTerminal is based on). The installer and the auto-updater use
# OTerminal's own version instead: the OTERMINAL_VERSION file, bumped by CI.
$oterminalVersionFile = Join-Path $env:ZED_WORKSPACE "OTERMINAL_VERSION"
if (Test-Path $oterminalVersionFile) {
    $env:RELEASE_VERSION = (Get-Content $oterminalVersionFile -Raw).Trim()
}
Write-Output "OTerminal version: $env:RELEASE_VERSION"
$innoDir = "$env:ZED_WORKSPACE\inno\$Architecture"
$debugArchive = "$CargoOutDir\oterminal-$env:RELEASE_VERSION-$env:ZED_RELEASE_CHANNEL.dbg.zip"

CheckEnvironmentVariables
PrepareForBundle
GenerateLicenses
BuildOTerminalAndItsFriends
if ($RemoteServer) {
    BuildRemoteServer
}
SignOTerminalAndItsFriends
ZipOTerminalDebug
if ($Architecture -ne "aarch64") {
    DownloadAMDGpuServices
}
DownloadConpty
CollectFiles
BuildInstaller

if ($buildSuccess) {
    Write-Output "Build successful"
    if ($Install) {
        Write-Output "Installing OTerminal..."
        Start-Process -FilePath $setupPath
    }
    exit 0
}
else {
    Write-Output "Build failed"
    exit 1
}
