param(
    [Parameter(Mandatory)] [string]$Executable,
    [Parameter(Mandatory)] [string]$ReleaseNotes,
    [Parameter(Mandatory)] [string]$Output
)

$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$version = (Get-Content -Raw -LiteralPath (Join-Path $root 'tauri.conf.json') | ConvertFrom-Json).version
if ($version -cnotmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') { throw 'Invalid stable release version.' }
$file = Get-Item -LiteralPath $Executable
if ($file.VersionInfo.ProductName -cne 'Workflow' -or $file.VersionInfo.ProductVersion -cne $version -or $file.Length -lt 64 -or $file.Length -gt 128MB) {
    throw 'The update executable does not match the intended release version or size.'
}
$publicText = [IO.File]::ReadAllText((Join-Path $root 'src/update-public-key.txt')).Trim()
$publicBlob = [Convert]::FromBase64String($publicText)
if ($publicBlob.Length -ne 72 -or [BitConverter]::ToUInt32($publicBlob, 0) -ne 0x31534345 -or
    [BitConverter]::ToUInt32($publicBlob, 4) -ne 32) { throw 'Invalid pinned update verification key.' }
$executableBytes = [IO.File]::ReadAllBytes($file.FullName)
if (![Text.Encoding]::UTF8.GetString($executableBytes).Contains($publicText)) {
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
    asset = 'Workflow-Update-Windows-x64.exe'
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
