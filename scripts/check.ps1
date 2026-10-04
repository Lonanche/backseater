param(
    [ValidateSet('all', 'fmt', 'clippy', 'test', 'release')]
    [string]$Check = 'all'
)

$ErrorActionPreference = 'Stop'

function Invoke-Cargo {
    param([string[]]$CargoArguments)

    Write-Host "Running cargo $($CargoArguments -join ' ')"
    & cargo @CargoArguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo $($CargoArguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

Push-Location (Split-Path -Parent $PSScriptRoot)
try {
    if ($Check -in @('all', 'fmt')) {
        Invoke-Cargo @('fmt', '--', '--check')
    }
    if ($Check -in @('all', 'clippy')) {
        Invoke-Cargo @('clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings')
    }
    if ($Check -in @('all', 'test')) {
        Invoke-Cargo @('test', '--workspace', '--locked')
    }
    if ($Check -eq 'release') {
        Invoke-Cargo @('build', '--release', '--locked', '-p', 'backseater')
    }
}
finally {
    Pop-Location
}
