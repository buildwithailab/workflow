param([string]$Executable)

$ErrorActionPreference = 'Stop'
$workflowRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$workflowExecutable = if ($Executable) { $Executable } else { Join-Path $workflowRoot 'Workflow.exe' }
if (-not (Test-Path -LiteralPath $workflowExecutable -PathType Leaf)) { throw 'Workflow.exe was not found. Run Build.cmd first.' }

# Check both byte alignments for UTF-16 Windows strings as well as UTF-8 paths.
$workflowBytes = [IO.File]::ReadAllBytes((Resolve-Path -LiteralPath $workflowExecutable).Path)
$workflowPrivatePath = '(?i)(?:[A-Z]:[\\/]+(?:Users|Documents and Settings)[\\/]+|/(?:home|Users)/)[^\\/\s\x00]+[\\/]'
$workflowHasPrivatePath = [Text.Encoding]::UTF8.GetString($workflowBytes) -match $workflowPrivatePath
for ($workflowOffset = 0; $workflowOffset -lt 2 -and $workflowOffset -lt $workflowBytes.Length; $workflowOffset++) {
    $workflowHasPrivatePath = $workflowHasPrivatePath -or
        [Text.Encoding]::Unicode.GetString($workflowBytes, $workflowOffset, $workflowBytes.Length - $workflowOffset) -match $workflowPrivatePath
}
if ($workflowHasPrivatePath) { throw 'The executable contains a local user path. Rebuild it with Build.cmd.' }

$workflowDistribution = Join-Path $workflowRoot 'dist\Workflow-Windows-x64'
New-Item -ItemType Directory -Force -Path $workflowDistribution | Out-Null
Copy-Item -LiteralPath $workflowExecutable -Destination (Join-Path $workflowDistribution 'Workflow.exe') -Force
$workflowDocs = @('Start-Workflow.cmd','README.md','LICENSE','THIRD-PARTY-NOTICES.txt')
foreach ($workflowFile in $workflowDocs) { Copy-Item -LiteralPath (Join-Path $workflowRoot $workflowFile) -Destination $workflowDistribution -Force }
$workflowNames = @('Workflow.exe') + $workflowDocs + @('LICENSE-xterm.txt','LICENSE-addon-fit.txt')
foreach($workflowName in @('LICENSE-xterm.txt','LICENSE-addon-fit.txt')) { Copy-Item -LiteralPath (Join-Path $workflowRoot "ui\vendor\$workflowName") -Destination $workflowDistribution -Force }
$workflowImages = @(
    'docs/images/issue-filter-example.svg',
    'docs/images/mcp-routing.svg',
    'docs/images/workflow-five-session-workspace.png',
    'docs/images/issue-filter-test-steps.png',
    'docs/images/workflow-browser-qa-walkthrough.png',
    'docs/images/workflow-inventory-element-picking.png',
    'docs/images/workflow-session-addons.png'
)
New-Item -ItemType Directory -Force -Path (Join-Path $workflowDistribution 'docs/images') | Out-Null
foreach ($workflowImage in $workflowImages) {
    Copy-Item -LiteralPath (Join-Path $workflowRoot $workflowImage) -Destination (Join-Path $workflowDistribution $workflowImage) -Force
}
$workflowNames += $workflowImages
$workflowManifest = $workflowNames | ForEach-Object { [PSCustomObject]@{ file = $_; sha256 = (Get-FileHash -LiteralPath (Join-Path $workflowDistribution $_) -Algorithm SHA256).Hash.ToLowerInvariant() } }
$workflowManifest | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $workflowDistribution 'manifest.json') -Encoding utf8
$workflowZip = Join-Path $workflowRoot 'dist\Workflow-Windows-x64.zip'
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$workflowTempZip = Join-Path $workflowRoot "dist\Workflow-$([guid]::NewGuid()).zip"
try {
    $workflowArchive = [IO.Compression.ZipFile]::Open($workflowTempZip, [IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($workflowName in ($workflowNames + @('manifest.json'))) {
            [void][IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                $workflowArchive, (Join-Path $workflowDistribution $workflowName), $workflowName
            )
        }
    } finally {
        $workflowArchive.Dispose()
    }
    Move-Item -LiteralPath $workflowTempZip -Destination $workflowZip -Force
} finally {
    if (Test-Path -LiteralPath $workflowTempZip) { Remove-Item -LiteralPath $workflowTempZip }
}
$workflowHash = Get-FileHash -LiteralPath $workflowZip -Algorithm SHA256
"$($workflowHash.Hash.ToLowerInvariant())  $([IO.Path]::GetFileName($workflowZip))" |
    Set-Content -LiteralPath "$workflowZip.sha256" -Encoding ascii
$workflowHash | Select-Object Algorithm,Hash,Path
