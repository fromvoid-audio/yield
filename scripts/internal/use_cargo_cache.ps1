<#
.SYNOPSIS
Configures optional local Cargo compile caching for repository scripts.

.DESCRIPTION
Cargo runs default to direct `rustc`. Scripts can opt in to `sccache` with
`YIELD_ENABLE_SCCACHE=1`, and can force the direct compiler path with
`YIELD_DISABLE_SCCACHE=1`. When `sccache` is enabled or already inherited
through `RUSTC_WRAPPER`, this helper probes it in wrapper mode and falls back
to direct `rustc` if the probe fails or times out.
#>

$script:YieldCargoConfigOverrideArgs = @()

function Get-YieldRustcPassthroughWrapperPath {
  return (Join-Path $PSScriptRoot "rustc_passthrough.cmd")
}

function Test-YieldWritableDirectory {
  param(
    [AllowEmptyString()]
    [string]$Path
  )

  if ([string]::IsNullOrWhiteSpace($Path) -or -not (Test-Path $Path -PathType Container)) {
    return $false
  }

  $probeFile = Join-Path $Path "yield-write-probe-$([guid]::NewGuid().ToString('N')).tmp"
  try {
    Set-Content -Path $probeFile -Value "probe" -NoNewline
    return $true
  } catch {
    return $false
  } finally {
    Remove-Item $probeFile -ErrorAction SilentlyContinue
  }
}

function Ensure-YieldWritableTempDir {
  $tempWritable = Test-YieldWritableDirectory $env:TEMP
  $tmpWritable = Test-YieldWritableDirectory $env:TMP
  if ($tempWritable -and $tmpWritable) {
    return
  }

  $fallback = Join-Path (Join-Path $PSScriptRoot "../..") "tmp/agent_temp"
  New-Item -ItemType Directory -Path $fallback -Force | Out-Null
  $env:TEMP = $fallback
  $env:TMP = $fallback
  Write-Host "[cargo-cache] using repo temp dir $fallback"
}

function Test-YieldSccacheWrapperValue {
  param(
    [AllowEmptyString()]
    [string]$Wrapper
  )

  if ([string]::IsNullOrWhiteSpace($Wrapper)) {
    return $false
  }

  $fileName = [System.IO.Path]::GetFileName($Wrapper).ToLowerInvariant()
  return $fileName -eq "sccache" -or $fileName -eq "sccache.exe"
}

function Get-YieldCargoConfigOverrideArgs {
  return $script:YieldCargoConfigOverrideArgs
}

function Invoke-YieldCargo {
  param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$Arguments
  )

  & cargo @(Get-YieldCargoConfigOverrideArgs) @Arguments
}

function Set-YieldCargoWrapperOverride {
  param(
    [AllowEmptyString()]
    [string]$Wrapper
  )

  if ([string]::IsNullOrWhiteSpace($Wrapper)) {
    Remove-Item Env:RUSTC_WRAPPER -ErrorAction SilentlyContinue
    Remove-Item Env:CARGO_BUILD_RUSTC_WRAPPER -ErrorAction SilentlyContinue
    $script:YieldCargoConfigOverrideArgs = @()
    return
  }

  $env:RUSTC_WRAPPER = $Wrapper
  $env:CARGO_BUILD_RUSTC_WRAPPER = $Wrapper
  $script:YieldCargoConfigOverrideArgs = @()
}

function Clear-YieldSccacheWrapper {
  param(
    [Parameter(Mandatory = $true)]
    [string]$Reason
  )

  if ((Test-YieldSccacheWrapperValue $env:RUSTC_WRAPPER) -or
      (Test-YieldSccacheWrapperValue $env:CARGO_BUILD_RUSTC_WRAPPER)) {
    Set-YieldCargoWrapperOverride ""
    Write-Host "[cargo-cache] $Reason; falling back to direct rustc"
    return
  }

  Write-Host "[cargo-cache] $Reason"
}

function Test-YieldSccacheHealth {
  param(
    [Parameter(Mandatory = $true)]
    [string]$SccachePath
  )

  $rustc = Get-Command rustc -ErrorAction SilentlyContinue
  if ($null -eq $rustc) {
    Write-Host "[cargo-cache] rustc not found; skipping sccache probe"
    return $false
  }

  try {
    $probeDir = Join-Path (Join-Path $PSScriptRoot "../..") "tmp/cargo_cache_probe"
    New-Item -ItemType Directory -Path $probeDir -Force | Out-Null
    $probeId = [guid]::NewGuid().ToString("N")
    $stdoutPath = Join-Path $probeDir "$probeId.stdout.log"
    $stderrPath = Join-Path $probeDir "$probeId.stderr.log"
  } catch {
    Write-Host "[cargo-cache] failed to prepare sccache probe files: $($_.Exception.Message)"
    return $false
  }

  try {
    $probe = Start-Process -FilePath $SccachePath `
      -ArgumentList @($rustc.Source, "--version") `
      -NoNewWindow `
      -PassThru `
      -RedirectStandardOutput $stdoutPath `
      -RedirectStandardError $stderrPath

    if (-not $probe.WaitForExit(5000)) {
      try {
        $probe.Kill()
      } catch {
      }
      Write-Host "[cargo-cache] sccache probe timed out"
      return $false
    }

    if ($probe.ExitCode -ne 0) {
      $stderr = Get-Content $stderrPath -Raw -ErrorAction SilentlyContinue
      $summary = ($stderr -split "\r?\n" |
          Where-Object { $_.Trim().Length -gt 0 } |
          Select-Object -First 1)
      if ([string]::IsNullOrWhiteSpace($summary)) {
        $summary = "exit code $($probe.ExitCode)"
      }
      Write-Host "[cargo-cache] sccache probe failed: $summary"
      return $false
    }

    return $true
  } finally {
    Remove-Item $stdoutPath, $stderrPath -ErrorAction SilentlyContinue
  }
}

function Enable-YieldCargoCache {
  Ensure-YieldWritableTempDir

  if ($env:YIELD_DISABLE_SCCACHE -eq "1") {
    Clear-YieldSccacheWrapper "sccache disabled by YIELD_DISABLE_SCCACHE=1"
    return
  }

  if ($env:YIELD_ENABLE_SCCACHE -ne "1") {
    Clear-YieldSccacheWrapper "sccache disabled by default"
    return
  }

  if (-not [string]::IsNullOrWhiteSpace($env:RUSTC_WRAPPER)) {
    if (Test-YieldSccacheWrapperValue $env:RUSTC_WRAPPER) {
      if (Test-YieldSccacheHealth $env:RUSTC_WRAPPER) {
        Set-YieldCargoWrapperOverride $env:RUSTC_WRAPPER
        Write-Host "[cargo-cache] keeping healthy existing RUSTC_WRAPPER=$($env:RUSTC_WRAPPER)"
        return
      }

      Clear-YieldSccacheWrapper "existing sccache wrapper failed health probe"
      return
    }

    Set-YieldCargoWrapperOverride $env:RUSTC_WRAPPER
    Write-Host "[cargo-cache] keeping existing RUSTC_WRAPPER=$($env:RUSTC_WRAPPER)"
    return
  }

  $sccache = Get-Command sccache -ErrorAction SilentlyContinue
  if ($null -eq $sccache) {
    return
  }

  if (-not (Test-YieldSccacheHealth $sccache.Source)) {
    Set-YieldCargoWrapperOverride ""
    Write-Host "[cargo-cache] skipping sccache because the wrapper probe failed"
    return
  }

  Set-YieldCargoWrapperOverride $sccache.Source
  Write-Host "[cargo-cache] using sccache via RUSTC_WRAPPER=$($env:RUSTC_WRAPPER)"
}
