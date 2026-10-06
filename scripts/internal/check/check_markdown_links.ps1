<#
.SYNOPSIS
Forward local Markdown link checks to the canonical Bash validator.
#>
param(
  [string]$Base,
  [string]$Head = "HEAD"
)
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$bash = Get-Command bash -ErrorAction SilentlyContinue
if ($null -eq $bash) { throw "Git Bash is required for Markdown link checks." }
$forward = @("--head", $Head)
if (-not [string]::IsNullOrWhiteSpace($Base)) { $forward += @("--base", $Base) }
$rootDir = (Resolve-Path (Join-Path $PSScriptRoot "../../..")).Path
Push-Location $rootDir
try {
  & $bash.Path "scripts/internal/check/check_markdown_links.sh" @forward
  $status = $LASTEXITCODE
} finally { Pop-Location }
exit $status
