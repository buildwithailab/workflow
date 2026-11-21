param(
    [Parameter(Mandatory)] [string]$Package,
    [Parameter(Mandatory)] [string]$ReleaseNotes,
    [Parameter(Mandatory)] [string]$Output
)

# Run on the designated Windows release account after Mac package acceptance.
# Prepares public assets only; never commits, pushes, changes latest or publishes.
$ErrorActionPreference = 'Stop'
$workflowRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$workflowVersion = (Get-Content -Raw -LiteralPath (Join-Path $workflowRoot 'tauri.conf.json') | ConvertFrom-Json).version
if ($workflowVersion -cnotmatch '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$') { throw 'Invalid stable release version.' }
$workflowOutput = [IO.Path]::GetFullPath($Output)
if (Test-Path -LiteralPath $workflowOutput) { throw 'Choose a new output directory; existing outputs are preserved.' }
[IO.Directory]::CreateDirectory($workflowOutput) | Out-Null
$workflowArchive = Join-Path $workflowOutput 'Workflow-Update-macOS-arm64.app.zip'
$workflowFeed = Join-Path $workflowOutput 'Workflow-Update-macOS-arm64.json'
$workflowSource = Get-Item -LiteralPath $Package
if ($workflowSource.Attributes -band [IO.FileAttributes]::ReparsePoint) { throw 'Package must be a regular file.' }
[IO.File]::Copy($workflowSource.FullName, $workflowArchive, $false)
& (Join-Path $PSScriptRoot 'sign-update.ps1') -Executable $workflowArchive -Platform macOS-arm64 -ReleaseNotes $ReleaseNotes -Output $workflowFeed
if (!$?) { throw 'Mac update signing failed.' }
$workflowDigest = (Get-FileHash -LiteralPath $workflowArchive -Algorithm SHA256).Hash.ToLowerInvariant()
$workflowEncoding = New-Object Text.UTF8Encoding($false)
[IO.File]::WriteAllText(($workflowArchive + '.sha256'), ($workflowDigest + '  Workflow-Update-macOS-arm64.app.zip' + "`n"), $workflowEncoding)
$workflowChannel = Join-Path $workflowOutput 'pages'
& node (Join-Path $PSScriptRoot 'macos-update-feed.cjs') $workflowFeed $workflowArchive ('v' + $workflowVersion + '-macos') $workflowChannel
if ($LASTEXITCODE -ne 0) { throw 'Mac update feed verification failed.' }
Write-Output 'Prepared ZIP, checksum and signed Mac manifest. Attach all three to a draft Mac release; publish with make_latest=false only after review. No release was published.'
