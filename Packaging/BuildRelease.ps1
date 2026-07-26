param(
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release'
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$launcherOutput = Join-Path $PSScriptRoot 'bin'
$releaseDirectory = ([char]0x53D1).ToString() + [char]0x5E03
$zipName = ([char]0x4FDD).ToString() + [char]0x536B + [char]0x841D + [char]0x535C +
    [char]0x7CFB + [char]0x5217 + [char]0x89E3 + [char]0x5305 + [char]0x5DE5 + [char]0x5177 + '-win-x64.zip'
$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path -LiteralPath $vswhere)) {
    throw 'Visual Studio C++ build tools are required to build the runtime launcher.'
}

$visualStudio = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ([string]::IsNullOrWhiteSpace($visualStudio)) {
    throw 'Visual Studio C++ build tools were not found.'
}

$vcvars = Join-Path $visualStudio 'VC\Auxiliary\Build\vcvars64.bat'
New-Item -ItemType Directory -Force -Path $launcherOutput | Out-Null
$compileCommand = 'call "' + $vcvars + '" >nul && rc /nologo /fo "' +
    (Join-Path $launcherOutput 'RuntimeLauncher.res') + '" "Packaging\RuntimeLauncher.rc" && cl /nologo /O2 /MT /DUNICODE /D_UNICODE /Fe:"' +
    (Join-Path $launcherOutput 'RuntimeLauncher.exe') + '" "Packaging\RuntimeLauncher.c" "' +
    (Join-Path $launcherOutput 'RuntimeLauncher.res') + '" user32.lib'
Push-Location $projectRoot
try {
    & cmd.exe /d /c $compileCommand
    if ($LASTEXITCODE -ne 0) {
        throw "Native launcher compilation failed: $LASTEXITCODE"
    }

    $releaseParent = [IO.Path]::GetFullPath((Join-Path $projectRoot $releaseDirectory))
    $releaseRoot = [IO.Path]::GetFullPath((Join-Path $releaseParent 'win-x64'))
    if (-not $releaseRoot.StartsWith($releaseParent + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Refusing to modify a directory outside the release folder.'
    }

    if (Test-Path -LiteralPath $releaseRoot) {
        Remove-Item -LiteralPath $releaseRoot -Recurse -Force
    }

    $runtime = Join-Path $releaseRoot 'runtime'
    New-Item -ItemType Directory -Force -Path $runtime | Out-Null
    & dotnet publish CarrotUnpacker.csproj -c $Configuration -r win-x64 --self-contained true -p:Platform=x64 -p:WindowsPackageType=None -p:WindowsAppSDKSelfContained=true "-p:PublishDir=$runtime\" --no-restore
    if ($LASTEXITCODE -ne 0) {
        throw "Publish failed: $LASTEXITCODE"
    }

    foreach ($file in @(
        'CarrotUnpacker.deps.json',
        'CarrotUnpacker.dll',
        'CarrotUnpacker.pri',
        'CarrotUnpacker.runtimeconfig.json')) {
        Copy-Item -LiteralPath (Join-Path $runtime $file) -Destination (Join-Path $releaseRoot $file)
    }

    Copy-Item -LiteralPath (Join-Path $launcherOutput 'RuntimeLauncher.exe') -Destination (Join-Path $releaseRoot 'CarrotUnpacker.exe')
    $zipPath = Join-Path $releaseParent $zipName
    Compress-Archive -LiteralPath $releaseRoot -DestinationPath $zipPath -CompressionLevel Optimal -Force
    Get-Item -LiteralPath $zipPath
}
finally {
    Pop-Location
}
