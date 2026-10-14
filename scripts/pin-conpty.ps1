# Put Microsoft's packaged ConPTY host (conpty.dll + OpenConsole.exe) beside
# the test binaries in each -Destination directory, so the pty tests run under a
# modern console host - the kind Windows Terminal ships - instead of the inbox
# conhost.exe. portable-pty loads a side-loaded conpty.dll in preference to the
# system one, and the paste tests require it (see apps/hrdr/tests/tui_pty.rs).
#
# Pinned by version and by the package's SHA-256, so CI never runs a host it
# has not checked.
param(
    [Parameter(Mandatory = $true)]
    [string[]] $Destination
)

$ErrorActionPreference = 'Stop'

$Version = '1.25.260930003'
$Sha256 = '02B07B349AF66D801159BDF9E440D4A1CE78BB951F37FC8609731665AFDAE7EE'
$Url = "https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/$Version/microsoft.windows.console.conpty.$Version.nupkg"

$work = Join-Path ([System.IO.Path]::GetTempPath()) "hrdr-conpty-$Version"
New-Item -ItemType Directory -Force -Path $work | Out-Null
# A .nupkg is a zip; Expand-Archive only accepts the .zip extension.
$package = Join-Path $work 'conpty.zip'
Invoke-WebRequest -Uri $Url -OutFile $package

$actual = (Get-FileHash -Path $package -Algorithm SHA256).Hash
if ($actual -ne $Sha256) {
    throw "ConPTY package hash mismatch: expected $Sha256, got $actual"
}

$extracted = Join-Path $work 'extracted'
Expand-Archive -Path $package -DestinationPath $extracted -Force
$dll = Join-Path $extracted 'runtimes\win-x64\native\conpty.dll'
$host_exe = Join-Path $extracted 'build\native\runtimes\x64\OpenConsole.exe'

foreach ($dir in $Destination) {
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Copy-Item -Force -Path $dll, $host_exe -Destination $dir
    Write-Host "pinned ConPTY $Version into $dir"
}
