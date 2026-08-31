<#
.SYNOPSIS
    Checks a staged distribution and the Inno Setup script before packaging.

.DESCRIPTION
    Run between `cargo dist` and `iscc`. The installer copies whatever is in
    the staged tree, so a file that failed to build there ships as a missing
    file in Program Files rather than as a build error -- this is where that
    becomes an error instead.

    It also reads installer\vmlord.iss back, because the two installation
    modes are the whole point of the packaging decision and a later edit that
    drops one of them is otherwise invisible until someone without
    administrator rights tries to install.

.EXAMPLE
    powershell -File installer\check.ps1 target\dist
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [string] $DistDir = (Join-Path $PSScriptRoot '..\target\dist')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$problems = New-Object System.Collections.Generic.List[string]

function Require-File {
    param([string] $Relative)

    $path = Join-Path $DistDir $Relative
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        $problems.Add("missing from the distribution: $Relative")
        return
    }
    if ((Get-Item -LiteralPath $path).Length -eq 0) {
        $problems.Add("empty in the distribution: $Relative")
    }
}

function Read-PeSubsystem {
    param([string] $Path)

    $bytes = [System.IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 64 -or $bytes[0] -ne 0x4d -or $bytes[1] -ne 0x5a) {
        throw 'not a PE file: missing DOS header'
    }

    $pe = [BitConverter]::ToInt32($bytes, 0x3c)
    $optional = $pe + 24
    $subsystem = $optional + 68
    if ($pe -lt 0 -or $subsystem + 2 -gt $bytes.Length -or
        $bytes[$pe] -ne 0x50 -or $bytes[$pe + 1] -ne 0x45 -or
        $bytes[$pe + 2] -ne 0 -or $bytes[$pe + 3] -ne 0) {
        throw 'not a PE file: missing PE header'
    }

    $magic = [BitConverter]::ToUInt16($bytes, $optional)
    if ($magic -ne 0x10b -and $magic -ne 0x20b) {
        throw "not a PE file: unknown optional-header magic 0x$($magic.ToString('x'))"
    }

    [BitConverter]::ToUInt16($bytes, $subsystem)
}

function Require-PeSubsystem {
    param(
        [string] $Relative,
        [uint16] $Expected,
        [string] $Name
    )

    $path = Join-Path $DistDir $Relative
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        return
    }
    try {
        $actual = Read-PeSubsystem $path
        if ($actual -ne $Expected) {
            $problems.Add("$Relative uses PE subsystem $actual, not $Name ($Expected)")
        }
    } catch {
        $problems.Add("cannot read the PE subsystem from ${Relative}: $($_.Exception.Message)")
    }
}

if (-not (Test-Path -LiteralPath $DistDir -PathType Container)) {
    Write-Error "no staged distribution at $DistDir; run ``cargo dist`` first"
    exit 1
}

# The binaries the application launches by name from beside itself.
Require-File 'vmlord.exe'
Require-File 'vmlord-com1.exe'
Require-File 'vmlord-ssh.exe'
Require-File 'vmlord-display.exe'
Require-PeSubsystem 'vmlord.exe' 2 'Windows GUI'
# The guest agent, which is copied into the VM rather than run on the host.
Require-File 'vmlord-agent'

# VMLord is GPL-licensed and the notices are generated from the dependency
# graph; both have to travel with the binaries.
Require-File 'LICENSE'
Require-File 'THIRD-PARTY-LICENSES.txt'

# The canonical distribution profiles the application copies into each user's
# own directory on startup. Ubuntu is the one profile the product is not
# usable without.
Require-File 'distros\ubuntu.json'

$script = Join-Path $PSScriptRoot 'vmlord.iss'
if (-not (Test-Path -LiteralPath $script -PathType Leaf)) {
    $problems.Add("missing installer script: $script")
} else {
    $text = Get-Content -LiteralPath $script -Raw

    # Both installation modes: `lowest` keeps the setup program from demanding
    # elevation, and `dialog` is what offers the all-users choice anyway.
    if ($text -notmatch '(?m)^\s*PrivilegesRequired\s*=\s*lowest\s*$') {
        $problems.Add('vmlord.iss does not set PrivilegesRequired=lowest')
    }
    if ($text -notmatch '(?m)^\s*PrivilegesRequiredOverridesAllowed\s*=\s*dialog\s*$') {
        $problems.Add('vmlord.iss does not set PrivilegesRequiredOverridesAllowed=dialog')
    }
    # `{autopf}` is what makes the chosen mode decide the directory; a literal
    # `{pf}` would install every per-user copy into Program Files. The name
    # after it may be the preprocessor variable the script actually uses.
    if ($text -notmatch '(?m)^\s*DefaultDirName\s*=\s*\{autopf\}\\(VMLord|\{#AppName\})\s*$') {
        $problems.Add('vmlord.iss does not install into {autopf}\VMLord')
    }

    # Uninstalling VMLord must never take the user's settings, VMs or images
    # with it. Only the directives are read: the section's comment says the
    # same thing in words, and matching that would report the promise as the
    # breach of it.
    $uninstall = [regex]::Match($text, '(?ims)^\s*\[UninstallDelete\]\s*$(.*?)(^\s*\[|\z)')
    if ($uninstall.Success) {
        $directives = $uninstall.Groups[1].Value -split '\r?\n' |
            Where-Object { $_.Trim() -ne '' -and -not $_.TrimStart().StartsWith(';') }
        foreach ($directive in $directives) {
            if ($directive -match '(?i)\{(localappdata|userappdata|appdata)\}') {
                $problems.Add("vmlord.iss would delete user data on uninstall:$directive")
            }
        }
    }
}

if ($problems.Count -gt 0) {
    foreach ($problem in $problems) {
        Write-Host "check: $problem"
    }
    # Write-Host and a status rather than Write-Error: this is a checklist, and
    # a PowerShell stack trace above the list only buries what it found.
    Write-Host "check: not ready to package -- $($problems.Count) problem(s)"
    exit 1
}

Write-Host "check: $DistDir is ready to package"
