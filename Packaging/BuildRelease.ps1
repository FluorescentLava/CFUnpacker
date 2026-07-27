param(
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release'
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$releaseDirectoryName = ([char]0x53D1).ToString() + [char]0x5E03
$zipName = 'CFUnpacker-win-x64.zip'
$rootFiles = @(
    'CFUnpacker.exe',
    'CFUnpacker.dll',
    'CFUnpacker.deps.json',
    'CFUnpacker.pri',
    'CFUnpacker.runtimeconfig.json'
)

function Get-LocalDotNetRuntime {
    $dotnetCommand = Get-Command dotnet -ErrorAction Stop
    $dotnetRoot = Split-Path -Parent $dotnetCommand.Source
    $frameworkRoot = Join-Path $dotnetRoot 'shared\Microsoft.NETCore.App'
    $fxrRoot = Join-Path $dotnetRoot 'host\fxr'

    $runtime = Get-ChildItem -LiteralPath $frameworkRoot -Directory |
        Where-Object {
            try {
                ([Version]$_.Name).Major -eq 10 -and
                    (Test-Path -LiteralPath (Join-Path $fxrRoot "$($_.Name)\hostfxr.dll"))
            }
            catch {
                $false
            }
        } |
        Sort-Object { [Version]$_.Name } -Descending |
        Select-Object -First 1

    if ($null -eq $runtime) {
        throw '.NET 10 x64 runtime files were not found beside the active dotnet host.'
    }

    [pscustomobject]@{
        Root = $dotnetRoot
        Version = $runtime.Name
        FrameworkDirectory = $runtime.FullName
        HostFxrPath = Join-Path $fxrRoot "$($runtime.Name)\hostfxr.dll"
    }
}

function Move-RuntimeFiles {
    param(
        [Parameter(Mandatory)] [string]$ReleaseRoot,
        [Parameter(Mandatory)] [string]$RuntimeDirectory
    )

    New-Item -ItemType Directory -Force -Path $RuntimeDirectory | Out-Null
    foreach ($item in @(Get-ChildItem -LiteralPath $ReleaseRoot -Force)) {
        if ($item.Name -eq 'runtime' -or $rootFiles -contains $item.Name) {
            continue
        }

        Move-Item -LiteralPath $item.FullName -Destination $RuntimeDirectory
    }
}

function Copy-LocalDotNetRuntime {
    param(
        [Parameter(Mandatory)] [pscustomobject]$Runtime,
        [Parameter(Mandatory)] [string]$Destination
    )

    $fxrDestination = Join-Path $Destination "host\fxr\$($Runtime.Version)"
    $frameworkDestination = Join-Path $Destination "shared\Microsoft.NETCore.App\$($Runtime.Version)"
    New-Item -ItemType Directory -Force -Path $fxrDestination, $frameworkDestination | Out-Null

    Copy-Item -LiteralPath $Runtime.HostFxrPath -Destination $fxrDestination
    Copy-Item -Path (Join-Path $Runtime.FrameworkDirectory '*') -Destination $frameworkDestination -Recurse

    foreach ($file in @('dotnet.exe', 'LICENSE.txt', 'ThirdPartyNotices.txt')) {
        $source = Join-Path $Runtime.Root $file
        if (Test-Path -LiteralPath $source -PathType Leaf) {
            Copy-Item -LiteralPath $source -Destination $Destination
        }
    }
}

function Set-RelocatedWinAppManifest {
    param(
        [Parameter(Mandatory)] [string]$ExecutablePath,
        [Parameter(Mandatory)] [string]$GeneratedManifest,
        [Parameter(Mandatory)] [string]$VisualStudioPath
    )

    [xml]$manifest = Get-Content -LiteralPath $GeneratedManifest -Raw
    foreach ($fileNode in $manifest.SelectNodes("//*[local-name()='file']")) {
        $name = $fileNode.GetAttribute('name')
        if (-not [string]::IsNullOrWhiteSpace($name) -and
            -not $name.StartsWith('runtime\', [StringComparison]::OrdinalIgnoreCase)) {
            $fileNode.SetAttribute('name', "runtime\$name")
        }
    }

    $temporaryManifest = Join-Path ([IO.Path]::GetTempPath()) "CFUnpacker-$([Guid]::NewGuid().ToString('N')).manifest"
    $settings = [Xml.XmlWriterSettings]::new()
    $settings.Encoding = [Text.UTF8Encoding]::new($false)
    $settings.Indent = $true
    $writer = [Xml.XmlWriter]::Create($temporaryManifest, $settings)
    try {
        $manifest.Save($writer)
    }
    finally {
        $writer.Dispose()
    }

    try {
        $vcvars = Join-Path $VisualStudioPath 'VC\Auxiliary\Build\vcvars64.bat'
        $command = 'call "' + $vcvars + '" >nul && mt.exe -nologo -manifest "' +
            $temporaryManifest + '" -outputresource:"' + $ExecutablePath + '";#1'
        & cmd.exe /d /c $command
        if ($LASTEXITCODE -ne 0) {
            throw "Manifest update failed: $LASTEXITCODE"
        }
    }
    finally {
        if (Test-Path -LiteralPath $temporaryManifest) {
            [IO.File]::Delete($temporaryManifest)
        }
    }
}

function Assert-ReleaseLayout {
    param(
        [Parameter(Mandatory)] [string]$ReleaseRoot,
        [Parameter(Mandatory)] [string]$RuntimeDirectory
    )

    $expected = @($rootFiles) + 'runtime'
    $actual = @(Get-ChildItem -LiteralPath $ReleaseRoot -Force | Select-Object -ExpandProperty Name)
    $unexpected = @($actual | Where-Object { $expected -notcontains $_ })
    $missing = @($expected | Where-Object { $actual -notcontains $_ })
    if ($unexpected.Count -gt 0 -or $missing.Count -gt 0) {
        throw "Invalid release root. Missing: $($missing -join ', '); unexpected: $($unexpected -join ', ')"
    }

    foreach ($file in $rootFiles) {
        if (Test-Path -LiteralPath (Join-Path $RuntimeDirectory $file)) {
            throw "Root application file was duplicated in runtime: $file"
        }
    }

    $framework = Get-ChildItem -LiteralPath (Join-Path $RuntimeDirectory 'shared\Microsoft.NETCore.App') -Directory |
        Select-Object -First 1
    if ($null -eq $framework -or
        -not (Test-Path -LiteralPath (Join-Path $framework.FullName 'coreclr.dll'))) {
        throw 'The bundled .NET runtime is incomplete.'
    }
}

$vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
if (-not (Test-Path -LiteralPath $vswhere)) {
    throw 'Visual Studio Build Tools are required to update the application manifest.'
}

$visualStudio = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if ([string]::IsNullOrWhiteSpace($visualStudio)) {
    throw 'Visual Studio C++ Build Tools were not found.'
}

Push-Location $projectRoot
try {
    $releaseParent = [IO.Path]::GetFullPath((Join-Path $projectRoot $releaseDirectoryName))
    $releaseRoot = [IO.Path]::GetFullPath((Join-Path $releaseParent 'win-x64'))
    if (-not $releaseRoot.StartsWith($releaseParent + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Refusing to modify a directory outside the release folder.'
    }

    if (Test-Path -LiteralPath $releaseRoot) {
        [IO.Directory]::Delete($releaseRoot, $true)
    }
    New-Item -ItemType Directory -Force -Path $releaseRoot | Out-Null

    & dotnet publish CFUnpacker.csproj -c $Configuration -r win-x64 --self-contained false `
        -p:Platform=x64 `
        "-p:PublishDir=$releaseRoot\" `
        -p:AppHostRelativeDotNet=runtime `
        -p:AppHostDotNetSearch=AppRelative `
        --no-restore
    if ($LASTEXITCODE -ne 0) {
        throw "Publish failed: $LASTEXITCODE"
    }

    $runtimeDirectory = Join-Path $releaseRoot 'runtime'
    Move-RuntimeFiles -ReleaseRoot $releaseRoot -RuntimeDirectory $runtimeDirectory
    Copy-LocalDotNetRuntime -Runtime (Get-LocalDotNetRuntime) -Destination $runtimeDirectory

    $generatedManifest = Get-ChildItem -LiteralPath (Join-Path $projectRoot "obj\x64\$Configuration") -File -Recurse -Filter 'app.manifest' |
        Where-Object { $_.FullName -like '*\Manifests\app.manifest' } |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -First 1 -ExpandProperty FullName
    if ([string]::IsNullOrWhiteSpace($generatedManifest)) {
        throw 'The generated Windows App SDK manifest was not found.'
    }

    Set-RelocatedWinAppManifest `
        -ExecutablePath (Join-Path $releaseRoot 'CFUnpacker.exe') `
        -GeneratedManifest $generatedManifest `
        -VisualStudioPath $visualStudio
    Assert-ReleaseLayout -ReleaseRoot $releaseRoot -RuntimeDirectory $runtimeDirectory

    $zipPath = Join-Path $releaseParent $zipName
    Compress-Archive -LiteralPath $releaseRoot -DestinationPath $zipPath -CompressionLevel Optimal -Force
    Get-Item -LiteralPath $zipPath
}
finally {
    Pop-Location
}
