#Requires -Version 7.0
<#
.SYNOPSIS
    Installs the ledger CLI on Windows.
.EXAMPLE
    ./install.ps1 -Version v0.1.1 -Prefix "$env:LOCALAPPDATA\ledger"
#>
[CmdletBinding()]
param(
    [string] $Version = 'latest',
    [string] $Prefix = (Join-Path $env:LOCALAPPDATA 'ledger'),
    [switch] $Quiet,
    [switch] $Force
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

enum Architecture {
    X64
    Arm64
}

class Release {
    [string] $Version
    [Architecture] $Arch

    Release([string] $version, [Architecture] $arch) {
        $this.Version = $version
        $this.Arch = $arch
    }

    [string] Url() {
        $name = "ledger_$($this.Version.TrimStart('v'))_$($this.Arch.ToString().ToLower())-windows.zip"
        return "https://example.com/example/ledger/releases/download/$($this.Version)/$name"
    }
}

function Get-Architecture {
    switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64' { return [Architecture]::X64 }
        'ARM64' { return [Architecture]::Arm64 }
        default { throw "unsupported CPU: $env:PROCESSOR_ARCHITECTURE" }
    }
}

$bin = Join-Path $Prefix 'bin'
if ((Test-Path (Join-Path $bin 'ledger.exe')) -and -not $Force) {
    Write-Host "ledger is already installed in $bin; pass -Force to replace it."
    return
}

$release = [Release]::new($Version, (Get-Architecture))
$archive = Join-Path ([IO.Path]::GetTempPath()) "ledger-$([guid]::NewGuid()).zip"
try {
    Invoke-WebRequest -Uri $release.Url() -OutFile $archive
    New-Item -ItemType Directory -Path $bin -Force | Out-Null
    Expand-Archive -Path $archive -DestinationPath $bin -Force
}
finally {
    Remove-Item -Path $archive -ErrorAction SilentlyContinue
}

$path = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not $path.Contains($bin)) {
    [Environment]::SetEnvironmentVariable('Path', "$path;$bin", 'User')
    Write-Host "added $bin to your PATH. Open a new terminal to use it."
}
Write-Host "installed ledger $Version to $bin" -ForegroundColor Green
