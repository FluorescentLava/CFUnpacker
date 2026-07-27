param(
    [ValidateSet('Debug', 'Release')]
    [string]$Configuration = 'Release'
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path -Parent $PSScriptRoot
$releaseDirectory = ([char]0x53D1).ToString() + [char]0x5E03
$zipName = 'CFUnpacker-win-x64.zip'
Push-Location $projectRoot
try {
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
    & dotnet publish CFUnpacker.csproj -c $Configuration -r win-x64 --self-contained true -p:Platform=x64 "-p:PublishDir=$runtime\" --no-restore
    if ($LASTEXITCODE -ne 0) {
        throw "Publish failed: $LASTEXITCODE"
    }

    foreach ($file in @(
        'CFUnpacker.exe',
        'CFUnpacker.deps.json',
        'CFUnpacker.dll',
        'CFUnpacker.pri',
        'CFUnpacker.runtimeconfig.json')) {
        Move-Item -LiteralPath (Join-Path $runtime $file) -Destination (Join-Path $releaseRoot $file)
    }

    $zipPath = Join-Path $releaseParent $zipName
    Compress-Archive -LiteralPath $releaseRoot -DestinationPath $zipPath -CompressionLevel Optimal -Force
    Get-Item -LiteralPath $zipPath
}
finally {
    Pop-Location
}
