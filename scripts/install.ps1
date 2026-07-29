# Giverny installer for Windows.
#
#   irm https://github.com/y0av/giverny/releases/latest/download/install.ps1 | iex
#
# Downloads the release binary, installs it to %LOCALAPPDATA%\Giverny\bin
# (override with $env:GIVERNY_BIN_DIR), and puts that directory on your PATH.

$ErrorActionPreference = 'Stop'

$repo    = 'y0av/giverny'
$binDir  = if ($env:GIVERNY_BIN_DIR) { $env:GIVERNY_BIN_DIR } else { "$env:LOCALAPPDATA\Giverny\bin" }
$version = if ($env:GIVERNY_VERSION) { $env:GIVERNY_VERSION } else { 'latest' }

if ([System.Environment]::Is64BitOperatingSystem -eq $false) {
    throw 'Giverny requires 64-bit Windows.'
}
$target = 'x86_64-pc-windows-msvc'
$asset  = "giverny-$target.zip"
$url    = if ($version -eq 'latest') {
    "https://github.com/$repo/releases/latest/download/$asset"
} else {
    "https://github.com/$repo/releases/download/$version/$asset"
}

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    Write-Host "downloading $asset"
    Invoke-WebRequest -Uri $url -OutFile (Join-Path $tmp $asset) -UseBasicParsing
    Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath $tmp -Force

    $exe = Join-Path $tmp 'giverny.exe'
    if (-not (Test-Path $exe)) { throw 'archive did not contain giverny.exe' }

    New-Item -ItemType Directory -Path $binDir -Force | Out-Null
    $dest = Join-Path $binDir 'giverny.exe'
    # A running exe cannot be overwritten on Windows; move it aside first.
    if (Test-Path $dest) {
        $old = "$dest.old"
        Remove-Item $old -ErrorAction SilentlyContinue
        try { Move-Item $dest $old -Force } catch { }
    }
    Move-Item $exe $dest -Force
    Write-Host "installed $dest"

    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($userPath -notlike "*$binDir*") {
        [Environment]::SetEnvironmentVariable('Path', "$userPath;$binDir", 'User')
        Write-Host "added $binDir to your PATH (restart your terminal to pick it up)"
    }
} finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

Write-Host ''
Write-Host "run: giverny        (and 'giverny doctor' if Claude states look wrong)"
