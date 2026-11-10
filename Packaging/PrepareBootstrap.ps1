param(
    [string]$DestinationDirectory
)

$ErrorActionPreference = 'Stop'
$projectRoot = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
if ([string]::IsNullOrWhiteSpace($DestinationDirectory)) {
    $DestinationDirectory = Join-Path $projectRoot 'target\release'
}
$destination = [IO.Path]::GetFullPath($DestinationDirectory)
$packageId = 'microsoft.windowsappsdk.foundation'
$version = '2.3.5'
$packageFile = "$packageId.$version.nupkg"
$expectedSha256 = '15D78449C8566F889E0F95AE6EBC0610D79B32F95CE296B621F54B8EC8F496CB'
$cacheDirectory = Join-Path $projectRoot 'artifacts\packages'
New-Item -ItemType Directory -Path $cacheDirectory -Force | Out-Null
$packagePath = Join-Path $cacheDirectory $packageFile

# Reuse a verified NuGet cache when available; no dotnet/NuGet SDK command is used.
if (-not (Test-Path -LiteralPath $packagePath -PathType Leaf)) {
    $localPackage = Join-Path $env:USERPROFILE ".nuget\packages\$packageId\$version\$packageFile"
    if (Test-Path -LiteralPath $localPackage -PathType Leaf) {
        Copy-Item -LiteralPath $localPackage -Destination $packagePath
    }
    else {
        $uri = "https://api.nuget.org/v3-flatcontainer/$packageId/$version/$packageFile"
        Invoke-WebRequest -Uri $uri -OutFile $packagePath -UseBasicParsing
    }
}
$actualHash = (Get-FileHash -LiteralPath $packagePath -Algorithm SHA256).Hash
if ($actualHash -ne $expectedSha256) {
    throw "Windows App SDK bootstrap package hash mismatch: $actualHash"
}

Add-Type -AssemblyName System.IO.Compression.FileSystem
New-Item -ItemType Directory -Path $destination -Force | Out-Null
$licenseDirectory = Join-Path $destination 'Licenses'
New-Item -ItemType Directory -Path $licenseDirectory -Force | Out-Null
$entries = @{
    'runtimes/win-x64/native/Microsoft.WindowsAppRuntime.Bootstrap.dll' = (Join-Path $destination 'Microsoft.WindowsAppRuntime.Bootstrap.dll')
    'license.txt' = (Join-Path $licenseDirectory 'Microsoft.WindowsAppSDK.Foundation.txt')
}
$archive = [IO.Compression.ZipFile]::OpenRead($packagePath)
try {
    foreach ($entryName in $entries.Keys) {
        $entry = $archive.GetEntry($entryName)
        if ($null -eq $entry) { throw "Missing bootstrap package entry: $entryName" }
        [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $entries[$entryName], $true)
    }
}
finally { $archive.Dispose() }

$bootstrapPath = Join-Path $destination 'Microsoft.WindowsAppRuntime.Bootstrap.dll'
$signature = Get-AuthenticodeSignature -LiteralPath $bootstrapPath
if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notlike '*CN=Microsoft Corporation*') {
    throw "The bootstrap DLL does not have a valid Microsoft signature: $($signature.Status)"
}
[pscustomobject]@{
    Path = $bootstrapPath
    Bytes = (Get-Item -LiteralPath $bootstrapPath).Length
    Package = "$packageId/$version"
    PackageSha256 = $expectedSha256
    Signature = $signature.Status.ToString()
}
