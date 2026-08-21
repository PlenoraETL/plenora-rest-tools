[CmdletBinding()]
param(
    [string]$ImageName = "plenora-rest-tools-verify"
)

$ErrorActionPreference = "Stop"
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$dockerfile = Join-Path $repositoryRoot "Dockerfile.verify"
$releaseDockerfile = Join-Path $repositoryRoot "Dockerfile.release"
$releaseMetadataPath = Join-Path $repositoryRoot "release-metadata.json"

& docker build --file $dockerfile --tag $ImageName $repositoryRoot
if ($LASTEXITCODE -ne 0) {
    throw "Local verification failed with exit code $LASTEXITCODE"
}

$releaseMetadata = Get-Content $releaseMetadataPath -Raw | ConvertFrom-Json
$sourceDateEpoch = [long]$releaseMetadata.source_date_epoch
if ($sourceDateEpoch -le 0) {
    throw "release-metadata.json source_date_epoch must be a positive integer"
}

$abi3Arguments = @(
    "build",
    "--file", $releaseDockerfile,
    "--target", "python-abi3",
    "--build-arg", "SOURCE_DATE_EPOCH=$sourceDateEpoch",
    "--tag", "$ImageName-python-abi3",
    $repositoryRoot
)
& docker @abi3Arguments
if ($LASTEXITCODE -ne 0) {
    throw "Python ABI3 compatibility verification failed with exit code $LASTEXITCODE"
}

Write-Host "Local verification completed successfully."
