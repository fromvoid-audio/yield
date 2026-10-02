<#
.SYNOPSIS
Forward to the canonical GPUI Bash runner; share its checks and input-fenced coordination.
#>
param([switch]$Help)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$bash = Get-Command bash -ErrorAction SilentlyContinue
if ($null -eq $bash) { throw "Git Bash is required for portable GPUI agent checks." }
$forward = @()
if ($Help) { $forward += "--help" }
$rootDir = (Resolve-Path (Join-Path $PSScriptRoot "../../..")).Path
Push-Location $rootDir
try {
  & $bash.Path "scripts/internal/agent/run_agent_ci_checks.sh" @forward
  $status = $LASTEXITCODE
} finally { Pop-Location }
exit $status
