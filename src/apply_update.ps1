$ErrorActionPreference = 'Stop'
$source = $env:WORKFLOW_UPDATE_SOURCE
$destination = $env:WORKFLOW_UPDATE_DESTINATION
$receipt = $env:WORKFLOW_UPDATE_RECEIPT
$directory = [IO.Path]::GetDirectoryName($destination)
$temporary = Join-Path $directory ('.workflow-update-' + [guid]::NewGuid() + '.tmp')
$backup = "$temporary.previous"
$result = 'failed'
$canRestart = $false
try {
    if ([IO.Path]::GetExtension($destination) -ine '.exe') {
        throw 'Invalid destination'
    }
    $parent = Get-Process -Id ([int]$env:WORKFLOW_UPDATE_PARENT) -ErrorAction SilentlyContinue
    if ($parent -and !$parent.WaitForExit(30000)) {
        throw 'Workflow is still running'
    }
    $installed = [Diagnostics.FileVersionInfo]::GetVersionInfo($destination)
    if ($installed.ProductName -cne 'Workflow' -or
        $installed.ProductVersion -cne $env:WORKFLOW_UPDATE_INSTALLED_VERSION) {
        throw 'Installed version changed'
    }
    $canRestart = $true
    [IO.File]::Copy($source, $temporary)
    $file = [Diagnostics.FileVersionInfo]::GetVersionInfo($temporary)
    $hasher = [Security.Cryptography.SHA256]::Create()
    $stream = $null
    try {
        $stream = [IO.File]::OpenRead($temporary)
        $digest = [BitConverter]::ToString($hasher.ComputeHash($stream)).Replace('-', '')
    } finally {
        $hasher.Dispose()
        if ($stream) { $stream.Dispose() }
    }
    if ($digest -ine $env:WORKFLOW_UPDATE_SHA256 -or
        $file.ProductName -cne 'Workflow' -or
        $file.ProductVersion -cne $env:WORKFLOW_UPDATE_VERSION) {
        throw 'Invalid update'
    }
    [IO.File]::Replace($temporary, $destination, $backup)
    $result = 'installed'
} catch {
    if (![IO.File]::Exists($destination) -and [IO.File]::Exists($backup)) {
        [IO.File]::Move($backup, $destination)
    }
} finally {
    [IO.File]::WriteAllText($receipt, $result)
    Remove-Item -LiteralPath $temporary,$source -ErrorAction SilentlyContinue
    if ($result -eq 'installed') {
        Remove-Item -LiteralPath $backup -ErrorAction SilentlyContinue
    }
}
if ($canRestart -and $env:WORKFLOW_UPDATE_RESTART -eq '1' -and [IO.File]::Exists($destination)) {
    try {
        Start-Process -FilePath $destination -WorkingDirectory $PWD.ProviderPath
    } catch {
        [IO.File]::WriteAllText($receipt, 'restart-failed')
    }
}
