[CmdletBinding()]
param(
    [string]$SourceDateEpoch = "",
    [switch]$SkipManifestCheck
)

$ErrorActionPreference = "Stop"
$repositoryRoot = [System.IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$releaseRoot = [System.IO.Path]::GetFullPath((Join-Path $repositoryRoot ".release"))

if ([System.IO.Path]::GetDirectoryName($releaseRoot) -ne $repositoryRoot) {
    throw "Refusing to use a release directory outside the repository."
}

$pythonCommand = Get-Command python -ErrorAction SilentlyContinue
if ($null -eq $pythonCommand) {
    $pythonCommand = Get-Command python3 -ErrorAction Stop
}

$releaseTool = Join-Path $PSScriptRoot "release.py"
$version = (& $pythonCommand.Source $releaseTool current-version | Select-Object -Last 1).Trim()
if ($LASTEXITCODE -ne 0) {
    throw "Version validation failed."
}

if ([string]::IsNullOrWhiteSpace($SourceDateEpoch)) {
    $SourceDateEpoch = (& $pythonCommand.Source $releaseTool source-date-epoch).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "Cannot read SOURCE_DATE_EPOCH from release-metadata.json."
    }
}
if ($SourceDateEpoch -notmatch "^[0-9]+$") {
    throw "SourceDateEpoch must be a Unix timestamp."
}

if (Test-Path -LiteralPath $releaseRoot) {
    Remove-Item -LiteralPath $releaseRoot -Recurse -Force
}
$firstBuild = New-Item -ItemType Directory -Path (Join-Path $releaseRoot "repro-a")
$secondBuild = New-Item -ItemType Directory -Path (Join-Path $releaseRoot "repro-b")
$distribution = New-Item -ItemType Directory -Path (Join-Path $repositoryRoot "dist") -Force

Get-ChildItem -LiteralPath $distribution.FullName -File | Remove-Item -Force

function Invoke-ReproducibleBuild {
    param([string]$Destination)

    $buildArguments = @(
        "build",
        "--no-cache",
        "--file", (Join-Path $repositoryRoot "Dockerfile.release"),
        "--target", "artifacts",
        "--build-arg", "SOURCE_DATE_EPOCH=$SourceDateEpoch",
        "--output", "type=local,dest=$Destination",
        $repositoryRoot
    )
    & docker @buildArguments
    if ($LASTEXITCODE -ne 0) {
        throw "Release build failed with exit code $LASTEXITCODE."
    }
}

Invoke-ReproducibleBuild -Destination $firstBuild.FullName
Invoke-ReproducibleBuild -Destination $secondBuild.FullName

$compareArguments = @(
    $releaseTool,
    "compare",
    $firstBuild.FullName,
    $secondBuild.FullName,
    "--copy-to",
    $distribution.FullName
)
& $pythonCommand.Source @compareArguments
if ($LASTEXITCODE -ne 0) {
    throw "The two release builds are not byte-for-byte reproducible."
}

$sbomName = "plenora-rest-tools-$version.spdx.json"
$syftArguments = @(
    "run", "--rm",
    "--volume", "$($repositoryRoot):/source:ro",
    "--volume", "$($distribution.FullName):/out",
    "anchore/syft@sha256:678bfa565b60f747aac0f8e964fe5588a24445b8d0a480e91f6efd70020dfbb0",
    "scan", "dir:/source",
    "--source-name", "plenora-rest-tools",
    "--source-version", $version,
    "--exclude", "./target/**",
    "--exclude", "./.release/**",
    "--exclude", "./dist/**",
    "--exclude", "./.git/**",
    "--output", "spdx-json@2.3=/out/$sbomName"
)
& docker @syftArguments
if ($LASTEXITCODE -ne 0) {
    throw "SBOM generation failed with exit code $LASTEXITCODE."
}

$sbomPath = Join-Path $distribution.FullName $sbomName
& $pythonCommand.Source $releaseTool normalize-sbom $sbomPath $version
if ($LASTEXITCODE -ne 0) {
    throw "SBOM normalization failed."
}

$checksumPath = Join-Path $distribution.FullName "SHA256SUMS"
& $pythonCommand.Source $releaseTool checksums $distribution.FullName $checksumPath
if ($LASTEXITCODE -ne 0) {
    throw "Checksum generation failed."
}

if (-not $SkipManifestCheck) {
    & $pythonCommand.Source $releaseTool check-manifest $distribution.FullName
    if ($LASTEXITCODE -ne 0) {
        throw "Adoption manifest verification failed."
    }
}

Write-Host "Release $version is reproducible."
Get-ChildItem -LiteralPath $distribution.FullName -File |
    Sort-Object Name |
    Select-Object Name, Length
