param(
    [ValidateSet('full', 'storage', 'session')]
    [string] $Scope = 'full'
)

$ErrorActionPreference = 'Stop'
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Push-Location $repoRoot
try {
    $head = (git rev-parse HEAD).Trim()
    if ($LASTEXITCODE -ne 0) { throw 'Cannot resolve HEAD' }
    Write-Host "[gpui-local-ci] host=Windows head=$head scope=$Scope"

    function Run-Cargo {
        param([string[]] $CargoArgs)
        Write-Host "+ cargo $($CargoArgs -join ' ')"
        & cargo @CargoArgs
        if ($LASTEXITCODE -ne 0) { throw "cargo failed with exit code $LASTEXITCODE" }
    }

    Run-Cargo -CargoArgs @('fmt', '--all', '--check')
    if ($Scope -eq 'session') {
        Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-session', '--lib')
        Write-Host "[gpui-local-ci] PASS head=$head scope=$Scope host=Windows"
        return
    }

    if ($Scope -eq 'full') {
        & (Join-Path $repoRoot 'scripts/internal/release/setup_windows_asio_sdk.ps1')
        if (-not $?) { throw 'ASIO setup failed' }
        Run-Cargo -CargoArgs @('check', '--locked', '-p', 'wavecrate-gpui')
    } else {
        Run-Cargo -CargoArgs @('check', '--locked', '-p', 'wavecrate-library', '--no-default-features')
    }

    Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-library', '--test', 'profile_admission', '--no-default-features')
    Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-library', '--lib', 'app_dirs::profile_ownership', '--no-default-features')
    Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-library', '--test', 'readiness_lease_process', '--features', 'test-support')
    Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-library', '--lib', 'operation_journal', '--no-default-features')
    Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-library', '--lib', 'file_ops_journal::reader', '--no-default-features')
    Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-library', '--lib', 'profile_mutation_guard', '--no-default-features')
    Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-library', '--lib', 'readiness', '--no-default-features')
    Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-library', '--lib', 'source_database_identity', '--no-default-features')
    Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-library', '--lib', 'sqlite_wal', '--no-default-features')
    Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-library', '--lib', 'app_dirs::storage_usage', '--no-default-features')
    Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-session', '-p', 'wavecrate-catalog', '--lib')

    if ($Scope -eq 'storage') {
        Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-core', '--lib', 'waveform_edit_effects')
        Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-media', '--lib', 'edit_intent')
    } else {
        Run-Cargo -CargoArgs @('test', '--locked', '-p', 'reson', '-p', 'wavecrate-core', '-p', 'wavecrate-media', '-p', 'wavecrate-platform', '--lib')
        Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-gpui', '-p', 'wavecrate-ui', '--no-run')
        Run-Cargo -CargoArgs @('test', '--locked', '-p', 'wavecrate-ui', '--lib')
    }

    Write-Host "[gpui-local-ci] PASS head=$head scope=$Scope host=Windows"
} finally {
    Pop-Location
}
