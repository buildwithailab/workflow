param([switch]$Create)

$ErrorActionPreference = 'Stop'
$keyName = 'Workflow.UpdateRelease.P256.v1'
$provider = [Security.Cryptography.CngProvider]::MicrosoftSoftwareKeyStorageProvider
$publicFile = Join-Path $PSScriptRoot '../src/update-public-key.txt'
if (![Security.Cryptography.CngKey]::Exists($keyName, $provider)) {
    if (!$Create) { throw 'The protected update signing key is missing. Run this script with -Create on the designated release account before building.' }
    if (Test-Path -LiteralPath $publicFile) { throw 'A verification key is already pinned. Restore the original release account; do not silently replace the key.' }
    $options = New-Object Security.Cryptography.CngKeyCreationParameters
    $options.Provider = $provider
    $options.KeyUsage = [Security.Cryptography.CngKeyUsages]::Signing
    $options.ExportPolicy = [Security.Cryptography.CngExportPolicies]::None
    $key = [Security.Cryptography.CngKey]::Create([Security.Cryptography.CngAlgorithm]::ECDsaP256, $keyName, $options)
} else {
    $key = [Security.Cryptography.CngKey]::Open($keyName, $provider)
}
try {
    $publicBlob = $key.Export([Security.Cryptography.CngKeyBlobFormat]::EccPublicBlob)
    if ($publicBlob.Length -ne 72 -or [BitConverter]::ToUInt32($publicBlob, 0) -ne 0x31534345 -or
        [BitConverter]::ToUInt32($publicBlob, 4) -ne 32 -or $key.ExportPolicy -ne [Security.Cryptography.CngExportPolicies]::None) {
        throw 'Unexpected update signing key type or export policy.'
    }
    $publicText = [Convert]::ToBase64String($publicBlob)
    if (Test-Path -LiteralPath $publicFile) {
        if ([IO.File]::ReadAllText($publicFile).Trim() -cne $publicText) { throw 'The local signing key does not match the pinned verification key.' }
    } else {
        [IO.File]::WriteAllText($publicFile, $publicText + "`n", (New-Object Text.UTF8Encoding($false)))
    }
    Write-Output 'Public update verification key prepared. The non-exportable signing key stays in the Windows user key store.'
} finally {
    $key.Dispose()
}
