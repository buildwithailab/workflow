param([switch]$Offline)

$ErrorActionPreference = 'Stop'
$workflowRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$workflowProfile = [Environment]::GetFolderPath('UserProfile')
$workflowCargo = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $workflowProfile '.cargo' }
$workflowRustup = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $workflowProfile '.rustup' }

# Use Cargo's encoded flag format so Windows paths containing spaces remain one argument.
$workflowFlags = @('-C', 'target-feature=+crt-static')
$workflowRemaps = [ordered]@{
    $workflowProfile = '/build-home'
    $workflowCargo = '/cargo'
    $workflowRustup = '/rustup'
    $workflowRoot = '/workflow'
}
foreach ($workflowEntry in $workflowRemaps.GetEnumerator()) {
    $workflowPrefix = [IO.Path]::GetFullPath($workflowEntry.Key).TrimEnd('\', '/')
    $workflowFlags += "--remap-path-prefix=$workflowPrefix=$($workflowEntry.Value)"
    $workflowFlags += "--remap-path-prefix=$($workflowPrefix.Replace('\', '/'))=$($workflowEntry.Value)"
}

$workflowPreviousFlags = $env:CARGO_ENCODED_RUSTFLAGS
$env:CARGO_ENCODED_RUSTFLAGS = $workflowFlags -join [char]31
Push-Location -LiteralPath $workflowRoot
try {
    $workflowArgs = @('build', '--release', '--features', 'custom-protocol', '--target', 'x86_64-pc-windows-msvc', '--locked')
    if ($Offline) { $workflowArgs += '--offline' }
    & cargo @workflowArgs
    exit $LASTEXITCODE
}
finally {
    Pop-Location
    $env:CARGO_ENCODED_RUSTFLAGS = $workflowPreviousFlags
}
