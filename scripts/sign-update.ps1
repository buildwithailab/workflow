param(
    [Parameter(Mandatory)] [string]$Executable,
    [Parameter(Mandatory)] [string]$ReleaseNotes,
    [Parameter(Mandatory)] [string]$Output,
    [ValidateSet('Windows-x64', 'macOS-arm64')] [string]$Platform = 'Windows-x64'
)

$ErrorActionPreference = 'Stop'

function Read-UpdateEntry([IO.Compression.ZipArchiveEntry]$Entry, [long]$Limit) {
    if ($Entry.Length -gt $Limit) { throw 'An application archive entry exceeds its limit.' }
    $stream = $Entry.Open()
    $memory = New-Object IO.MemoryStream
    try {
        $buffer = New-Object byte[] 65536
        while (($count = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
            if ($memory.Length + $count -gt $Limit) { throw 'Expanded application archive entry exceeds its limit.' }
            $memory.Write($buffer, 0, $count)
        }
        if ($memory.Length -ne $Entry.Length) { throw 'Application archive entry size does not match.' }
        return ,($memory.ToArray())
    } finally { $stream.Dispose(); $memory.Dispose() }
}

function Read-UpdatePackageBytes([string]$Path, [string]$Version, [bool]$WindowsPackage) {
    # Keep the path stable while checking Windows metadata and capturing the
    # bytes. All later validation and signing use this one captured snapshot.
    $stream = [IO.File]::Open($Path, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    try {
        if ($stream.Length -lt 64 -or $stream.Length -gt 128MB) {
            throw 'The update package does not match the intended release size.'
        }
        if ($WindowsPackage) {
            $info = [Diagnostics.FileVersionInfo]::GetVersionInfo($Path)
            if ($info.ProductName -cne 'Workflow' -or $info.ProductVersion -cne $Version) {
                throw 'The update executable does not match the intended release version.'
            }
        }
        $captured = New-Object byte[] ([int]$stream.Length)
        $offset = 0
        while ($offset -lt $captured.Length) {
            $count = $stream.Read($captured, $offset, $captured.Length - $offset)
            if ($count -eq 0) { throw 'The update package changed while being captured.' }
            $offset += $count
        }
        if ($stream.ReadByte() -ne -1) { throw 'The update package changed while being captured.' }
        return ,$captured
    } finally { $stream.Dispose() }
}

function Assert-MacUpdatePackage([byte[]]$ArchiveBytes, [string]$Version, [string]$PublicText) {
    if ($ArchiveBytes.LongLength -lt 64 -or $ArchiveBytes.LongLength -gt 128MB) {
        throw 'The update package does not match the intended release size.'
    }
    Add-Type -AssemblyName System.IO.Compression
    $archiveMemory = [IO.MemoryStream]::new($ArchiveBytes, $false)
    $archive = $null
    try {
        $archive = [IO.Compression.ZipArchive]::new($archiveMemory, [IO.Compression.ZipArchiveMode]::Read, $false)
        if ($archive.Entries.Count -gt 8192) { throw 'Application archive contains too many entries.' }
        $names = New-Object 'Collections.Generic.HashSet[string]' ([StringComparer]::OrdinalIgnoreCase)
        [long]$expanded = 0
        foreach ($entry in $archive.Entries) {
            $name = $entry.FullName
            if ($name -match '[\\:\x00-\x1f\x7f]' -or $name.Contains('//') -or $name.StartsWith('/') -or
                $name.Split('/') -contains '..' -or $name.Split('/') -contains '.' -or
                !$names.Add($name.Normalize([Text.NormalizationForm]::FormC)) -or
                !$name.StartsWith('Workflow.app/', [StringComparison]::Ordinal)) {
                throw 'Application archive contains an unsafe or unexpected path.'
            }
            foreach ($part in $name.Split('/')) {
                if ($part -match '^(?:\.env.*|\.git|\.codex|\.claude|\.copilot|accounts|profiles|sessions|transcripts|leases|ui-profile|browser-profiles|teams-profile|Cookies|Login Data|Local State|auth(?:-status)?\.json|(?:\.)?credentials(?:\..*)?\.json|workspace\.json|ready\.json)$' -or
                    $part -match '\.(?:pem|key|pfx|p12|log|jsonl|sqlite[^.]*|db(?:-wal|-shm)?)$') {
                    throw 'Application archive contains local data or credentials.'
                }
            }
            if ((($entry.ExternalAttributes -shr 16) -band 0xf000) -eq 0xa000) { throw 'Symbolic links are not accepted in this application update package.' }
            $expanded += $entry.Length
            if ($expanded -gt 1GB) { throw 'Expanded application archive exceeds its limit.' }
        }
        $plistEntry = $archive.GetEntry('Workflow.app/Contents/Info.plist')
        if (!$plistEntry) { throw 'Application update is missing its Info.plist.' }
        $plistBytes = Read-UpdateEntry $plistEntry 1MB
        $memory = New-Object IO.MemoryStream(,$plistBytes)
        $settings = New-Object Xml.XmlReaderSettings
        # Apple plists normally declare a DOCTYPE. Ignore it without resolving
        # external resources or expanding user-defined entities.
        $settings.DtdProcessing = [Xml.DtdProcessing]::Ignore
        $settings.XmlResolver = $null
        $reader = [Xml.XmlReader]::Create($memory, $settings)
        try {
            $plist = New-Object Xml.XmlDocument
            $plist.XmlResolver = $null
            $plist.Load($reader)
        } finally { $reader.Dispose(); $memory.Dispose() }
        $fields = @{}
        foreach ($key in $plist.SelectNodes('/plist/dict/key')) {
            if ($fields.ContainsKey($key.InnerText)) { throw 'Duplicate application metadata field.' }
            $fields[$key.InnerText] = $null
            $value = $key.NextSibling
            while ($value -and $value.NodeType -ne [Xml.XmlNodeType]::Element) { $value = $value.NextSibling }
            if ($value -and $value.Name -eq 'string') { $fields[$key.InnerText] = $value.InnerText }
        }
        if ($fields['CFBundleIdentifier'] -cne 'local.workflow.desktop' -or
            $fields['CFBundleShortVersionString'] -cne $Version -or $fields['CFBundleVersion'] -cne $Version -or
            $fields['CFBundleExecutable'] -cnotin @('workflow', 'Workflow')) { throw 'Application metadata does not match the intended release.' }
        $binary = $archive.GetEntry('Workflow.app/Contents/MacOS/' + $fields['CFBundleExecutable'])
        if (!$binary) { throw 'Application executable was not found.' }
        $bytes = Read-UpdateEntry $binary 128MB
        if ($bytes.Length -lt 32 -or [BitConverter]::ToString($bytes, 0, 4) -cne 'CF-FA-ED-FE' -or
            [BitConverter]::ToUInt32($bytes, 4) -ne 0x0100000c -or [BitConverter]::ToUInt32($bytes, 12) -ne 2 -or
            ![Text.Encoding]::UTF8.GetString($bytes).Contains($PublicText)) {
            throw 'Application executable does not match arm64 or the pinned update verification key.'
        }
    } finally { if ($archive) { $archive.Dispose() }; $archiveMemory.Dispose() }
}

$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$version = (Get-Content -Raw -LiteralPath (Join-Path $root 'tauri.conf.json') | ConvertFrom-Json).version
if ($version -cnotmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') { throw 'Invalid stable release version.' }
$file = Get-Item -LiteralPath $Executable
$executableBytes = Read-UpdatePackageBytes $file.FullName $version ($Platform -eq 'Windows-x64')
$publicText = [IO.File]::ReadAllText((Join-Path $root 'src/update-public-key.txt')).Trim()
$publicBlob = [Convert]::FromBase64String($publicText)
if ($publicBlob.Length -ne 72 -or [BitConverter]::ToUInt32($publicBlob, 0) -ne 0x31534345 -or
    [BitConverter]::ToUInt32($publicBlob, 4) -ne 32) { throw 'Invalid pinned update verification key.' }
if ($Platform -eq 'macOS-arm64') {
    Assert-MacUpdatePackage $executableBytes $version $publicText
} elseif (![Text.Encoding]::UTF8.GetString($executableBytes).Contains($publicText)) {
    throw 'The executable does not contain the pinned verification key. Rebuild after initializing update signing.'
}
$notesFile = Get-Item -LiteralPath $ReleaseNotes
if ($notesFile.Length -gt 192KB) { throw 'Release notes are too large.' }
$utf8 = New-Object Text.UTF8Encoding($false, $true)
$notes = [IO.File]::ReadAllText($notesFile.FullName, $utf8)
if ($notes.Length -gt 40000 -or $notes -match '[\x00-\x08\x0b\x0c\x0e-\x1f\x7f-\x9f]') { throw 'Invalid release notes.' }
$digest = [Security.Cryptography.SHA256]::Create()
try { $executableHash = ([BitConverter]::ToString($digest.ComputeHash($executableBytes))).Replace('-', '').ToLowerInvariant() }
finally { $digest.Dispose() }
$payload = [ordered]@{
    schema = 1
    repository = 'buildwithailab/workflow'
    channel = 'stable'
    version = $version
    asset = $(if ($Platform -eq 'macOS-arm64') { 'Workflow-Update-macOS-arm64.app.zip' } else { 'Workflow-Update-Windows-x64.exe' })
    size = $executableBytes.LongLength
    sha256 = $executableHash
    notes = $notes
} | ConvertTo-Json -Compress
$bytes = $utf8.GetBytes($payload)
if ($bytes.Length -gt 192KB) { throw 'Update metadata is too large.' }
$keyName = 'Workflow.UpdateRelease.P256.v1'
$provider = [Security.Cryptography.CngProvider]::MicrosoftSoftwareKeyStorageProvider
if (![Security.Cryptography.CngKey]::Exists($keyName, $provider)) { throw 'The protected release signing key is unavailable on this Windows account.' }
$key = [Security.Cryptography.CngKey]::Open($keyName, $provider)
try {
    if ($key.ExportPolicy -ne [Security.Cryptography.CngExportPolicies]::None -or
        [Convert]::ToBase64String($key.Export([Security.Cryptography.CngKeyBlobFormat]::EccPublicBlob)) -cne $publicText) {
        throw 'The protected signing key does not match the pinned verification key.'
    }
    $signer = New-Object Security.Cryptography.ECDsaCng($key)
    try {
        $signature = $signer.SignData($bytes, [Security.Cryptography.HashAlgorithmName]::SHA256)
        if ($signature.Length -ne 64 -or !$signer.VerifyData($bytes, $signature, [Security.Cryptography.HashAlgorithmName]::SHA256)) {
            throw 'Update signature verification failed.'
        }
        $envelope = [ordered]@{algorithm='ecdsa-p256-sha256';payload=[Convert]::ToBase64String($bytes);signature=[Convert]::ToBase64String($signature)} | ConvertTo-Json -Compress
        $outputBytes = $utf8.GetBytes($envelope)
        if ($outputBytes.Length -gt 272KB) { throw 'Signed update metadata is too large.' }
        $destination = [IO.Path]::GetFullPath($Output)
        [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($destination)) | Out-Null
        $temporary = $destination + '.' + [guid]::NewGuid() + '.tmp'
        try {
            [IO.File]::WriteAllBytes($temporary, $outputBytes)
            Move-Item -LiteralPath $temporary -Destination $destination -Force
        } finally { if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary } }
        Write-Output 'Signed and verified update metadata. No private key was exported.'
    } finally { $signer.Dispose() }
} finally { $key.Dispose() }
