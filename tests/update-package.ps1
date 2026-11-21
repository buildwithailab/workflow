$ErrorActionPreference = 'Stop'
$workflowRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

# Load validation functions only. Never open or use the release signing key.
$parseTokens = $null
$parseErrors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile(
    (Join-Path $workflowRoot 'scripts/sign-update.ps1'), [ref]$parseTokens, [ref]$parseErrors)
if ($parseErrors.Count) { throw 'Signer syntax is invalid.' }
foreach ($function in $ast.FindAll({ param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] }, $false)) {
    . ([scriptblock]::Create($function.Extent.Text))
}

$temporaryParent = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$temporary = Join-Path $temporaryParent ('workflow-update-package-' + [guid]::NewGuid())
[IO.Directory]::CreateDirectory($temporary) | Out-Null
$public = [guid]::NewGuid().ToString()
$version = '1.2.3'

function New-SyntheticPackage([string]$Case) {
    $path = Join-Path $temporary ($Case + '.zip')
    $archive = [IO.Compression.ZipFile]::Open($path, [IO.Compression.ZipArchiveMode]::Create)
    try {
        $bundleVersion = if ($Case -eq 'wrong-version') { '1.2.4' } else { $version }
        $identifier = if ($Case -eq 'wrong-identity') { 'example.other' } else { 'local.workflow.desktop' }
        $declaration = if ($Case -eq 'external-entity') {
            '<!DOCTYPE plist [<!ENTITY blocked SYSTEM "file:///unavailable-synthetic-resource">]>'
        } else { '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">' }
        if ($Case -eq 'external-entity') { $identifier = '&blocked;' }
        $plist = '<?xml version="1.0" encoding="UTF-8"?>' + $declaration + '<plist version="1.0"><dict>' +
            '<key>CFBundleIdentifier</key><string>' + $identifier + '</string>' +
            '<key>CFBundleShortVersionString</key><string>' + $bundleVersion + '</string>' +
            '<key>CFBundleVersion</key><string>' + $bundleVersion + '</string>' +
            '<key>CFBundleExecutable</key><string>workflow</string></dict></plist>'
        if ($Case -eq 'duplicate-metadata') {
            $plist = $plist.Replace('<dict>', '<dict><key>CFBundleVersion</key><array/>')
        }
        $metadata = $archive.CreateEntry('Workflow.app/Contents/Info.plist').Open()
        try {
            $bytes = [Text.Encoding]::UTF8.GetBytes($plist)
            $metadata.Write($bytes, 0, $bytes.Length)
        } finally { $metadata.Dispose() }
        $marker = if ($Case -eq 'wrong-key') { [guid]::NewGuid().ToString() } else { $public }
        $bytes = New-Object byte[] (32 + $marker.Length)
        [byte[]]$header = @(0xcf, 0xfa, 0xed, 0xfe)
        [Array]::Copy($header, $bytes, 4)
        [Array]::Copy([BitConverter]::GetBytes([uint32]0x0100000c), 0, $bytes, 4, 4)
        [Array]::Copy([BitConverter]::GetBytes([uint32]2), 0, $bytes, 12, 4)
        if ($Case -eq 'wrong-cpu') { $bytes[4] = 7 }
        [Array]::Copy([Text.Encoding]::UTF8.GetBytes($marker), 0, $bytes, 32, $marker.Length)
        $binary = $archive.CreateEntry('Workflow.app/Contents/MacOS/workflow').Open()
        try { $binary.Write($bytes, 0, $bytes.Length) } finally { $binary.Dispose() }
        $extra = switch ($Case) {
            'traversal' { 'Workflow.app/../outside' }
            'foreign-root' { 'Other.app/Contents/example' }
            'duplicate-case' { 'workflow.app/contents/info.plist' }
            'environment' { 'Workflow.app/Contents/Resources/.env' }
            'account-data' { 'Workflow.app/Contents/Resources/accounts/local' }
            'browser-data' { 'Workflow.app/Contents/Resources/Cookies' }
            'repository-data' { 'Workflow.app/Contents/Resources/.git/config' }
            'symlink' { 'Workflow.app/Contents/Resources/link' }
        }
        if ($extra) {
            $entry = $archive.CreateEntry($extra)
            if ($Case -eq 'symlink') { $entry.ExternalAttributes = 0xa000 -shl 16 }
        }
    } finally { $archive.Dispose() }
    return $path
}

try {
    $validPath = New-SyntheticPackage 'valid'
    $captured = Read-UpdatePackageBytes $validPath $version $false
    Assert-MacUpdatePackage $captured $version $public
    # Replacing the path after capture must not change what is validated or
    # hashed. A fresh capture must reject the replacement's different key.
    $replacement = New-SyntheticPackage 'wrong-key'
    Copy-Item -LiteralPath $replacement -Destination $validPath -Force
    Assert-MacUpdatePackage $captured $version $public
    $replacementRejected = $false
    try { Assert-MacUpdatePackage (Read-UpdatePackageBytes $validPath $version $false) $version $public }
    catch { $replacementRejected = $true }
    if (!$replacementRejected) { throw 'A replaced archive was not validated independently.' }
    $rejected = 0
    foreach ($case in @('wrong-version', 'wrong-identity', 'wrong-key', 'wrong-cpu', 'external-entity',
        'traversal', 'foreign-root', 'duplicate-case', 'duplicate-metadata', 'environment', 'account-data',
        'browser-data', 'repository-data', 'symlink')) {
        $failed = $false
        try { Assert-MacUpdatePackage (Read-UpdatePackageBytes (New-SyntheticPackage $case) $version $false) $version $public }
        catch { $failed = $true }
        if (!$failed) { throw ('Package validation failed to reject category: ' + $case) }
        $rejected++
    }
    Write-Output ('PASS immutable update snapshot and synthetic Mac update package; rejected ' + $rejected + ' invalid package categories; no signing key accessed')
} finally {
    $resolved = [IO.Path]::GetFullPath($temporary)
    if (!$resolved.StartsWith($temporaryParent, [StringComparison]::OrdinalIgnoreCase) -or
        [IO.Path]::GetFileName($resolved) -notmatch '^workflow-update-package-[0-9a-f-]{36}$') {
        throw 'Temporary test directory could not be verified for cleanup.'
    }
    Remove-Item -LiteralPath $resolved -Recurse -Force
}
