[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$WheelDirectory,
    [string]$Python = "python"
)

# Installs the single Windows abi3 wheel found in $WheelDirectory into a fresh
# virtual environment of $Python and runs the SDK suite against it.
#
# The suite runs from a directory outside the checkout, so `plenora_rest` can
# only be imported from the installed wheel, never from python/ in the source
# tree. Exactly one wheel must be present: picking "the first" of several
# would test an artifact chosen by directory order.

$ErrorActionPreference = "Stop"
$repositoryRoot = Split-Path -Parent $PSScriptRoot
$tests = Join-Path $repositoryRoot "python\tests"

$wheels = @(Get-ChildItem -LiteralPath $WheelDirectory -Filter "plenora_rest-*-cp310-abi3-win_amd64.whl")
if ($wheels.Count -ne 1) {
    throw "expected exactly one cp310-abi3-win_amd64 wheel in $WheelDirectory, found $($wheels.Count)"
}

$workRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("plenora-rest-wheel-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $workRoot | Out-Null
try {
    $environment = Join-Path $workRoot "venv"
    & $Python -m venv $environment
    if ($LASTEXITCODE -ne 0) { throw "cannot create a virtual environment with $Python" }
    $venvPython = Join-Path $environment "Scripts\python.exe"
    & $venvPython -c "import sys; print(sys.version)"
    & $venvPython -m pip install --disable-pip-version-check --no-cache-dir --no-deps $wheels[0].FullName
    if ($LASTEXITCODE -ne 0) { throw "wheel installation failed" }

    Push-Location $workRoot
    try {
        & $venvPython -m unittest discover -s $tests -v
        if ($LASTEXITCODE -ne 0) { throw "SDK suite failed against the installed wheel" }
    }
    finally {
        Pop-Location
    }
}
finally {
    Remove-Item -LiteralPath $workRoot -Recurse -Force -ErrorAction SilentlyContinue
}
