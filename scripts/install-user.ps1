param(
  [string]$Root = "$HOME\.local"
)

$ErrorActionPreference = "Stop"
$repoDir = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$claudemonHome = if ($env:CLAUDEMON_HOME -and $env:CLAUDEMON_HOME.Trim()) {
  $env:CLAUDEMON_HOME
} else {
  Join-Path $HOME ".claudemon"
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
  throw "cargo not found in PATH. Install Rust first: https://rustup.rs"
}

if (Test-Path -LiteralPath $claudemonHome) {
  $homeItem = Get-Item -LiteralPath $claudemonHome -Force
  if ($homeItem.Attributes -band [IO.FileAttributes]::ReparsePoint) {
    throw "Refusing to use CLAUDEMON_HOME ($claudemonHome): symlink/reparse point is not allowed."
  }
  if (-not $homeItem.PSIsContainer) {
    throw "Refusing to use CLAUDEMON_HOME ($claudemonHome): expected a directory."
  }
} else {
  New-Item -ItemType Directory -Path $claudemonHome | Out-Null
}

cargo install --path $repoDir --locked --force --root $Root

$binDir = Join-Path $Root "bin"
Write-Host "Installed claudemon to $(Join-Path $binDir 'claudemon.exe')"
Write-Host "Prepared CLAUDEMON_HOME at $claudemonHome"

$pathEntries = $env:PATH -split ";"
if (-not ($pathEntries -contains $binDir)) {
  Write-Host "Add to PATH: $binDir"
}
