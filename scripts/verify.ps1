[CmdletBinding()]
param(
    [string]$ImageName = "plenora-rest-tools-verify"
)

$ErrorActionPreference = "Stop"
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$dockerfile = Join-Path $repositoryRoot "Dockerfile.verify"

& docker build --file $dockerfile --tag $ImageName $repositoryRoot
if ($LASTEXITCODE -ne 0) {
    throw "Local verification failed with exit code $LASTEXITCODE"
}

Write-Host "Local verification completed successfully."
