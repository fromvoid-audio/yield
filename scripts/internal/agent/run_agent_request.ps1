<#
.SYNOPSIS
Forward to the canonical GPUI Bash runner; share its checks and input-fenced coordination.
#>
param([switch]$SkipCi, [switch]$QuickCi, [switch]$FullCi, [switch]$Help)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$bash = Get-Command bash -ErrorAction SilentlyContinue
if ($null -eq $bash) { throw "Git Bash is required for portable GPUI agent checks." }
$forward = @()
if ($SkipCi) { $forward += "--skip-ci" }
if ($QuickCi) { $forward += "--quick-ci" }
if ($FullCi) { $forward += "--full-ci" }
if ($Help) { $forward += "--help" }
$rootDir = (Resolve-Path (Join-Path $PSScriptRoot "../../..")).Path
Push-Location $rootDir
try {
  & $bash.Path "scripts/internal/agent/run_agent_request.sh" @forward
  $status = $LASTEXITCODE
} finally { Pop-Location }
exit $status
