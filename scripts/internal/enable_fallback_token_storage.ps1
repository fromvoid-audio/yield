# Enable fallback token storage for yield
# This sets user-level environment variables permanently

$bytes = [byte[]]::new(32)
[System.Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
$secret = ($bytes | ForEach-Object { $_.ToString("x2") }) -join ""

[System.Environment]::SetEnvironmentVariable("YIELD_ALLOW_FALLBACK_TOKEN_STORAGE", "1", "User")
[System.Environment]::SetEnvironmentVariable("YIELD_FALLBACK_KEY", $secret, "User")

Write-Host "✓ Fallback token storage enabled permanently for your user account"
Write-Host "✓ YIELD_ALLOW_FALLBACK_TOKEN_STORAGE = 1"
Write-Host "✓ YIELD_FALLBACK_KEY = (set to random secret)"
Write-Host ""
Write-Host "Note: You may need to restart yield for these changes to take effect."
