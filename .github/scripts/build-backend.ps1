[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateSet('x86_64-pc-windows-msvc', 'x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu')]
    [string]$Target
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '../..')).Path
Push-Location -LiteralPath $repositoryRoot

function Invoke-CargoLogged {
    param([string[]]$CargoArguments)

    & cargo +stable @CargoArguments 2>&1 | Tee-Object -FilePath build.log -Append
    $buildExitCode = $LASTEXITCODE
    if ($buildExitCode -ne 0) {
        Get-Content -LiteralPath build.log -Tail 40
        exit $buildExitCode
    }
}

try {
    $windowsTarget = $Target -eq 'x86_64-pc-windows-msvc'
    if (($windowsTarget -and -not $IsWindows) -or (-not $windowsTarget -and -not $IsLinux)) {
        throw "Run $Target on its native Windows or Linux runner."
    }

    Invoke-CargoLogged -CargoArguments @('test', '--locked', '--target', $Target, '--', '--nocapture')
    Invoke-CargoLogged -CargoArguments @('build', '--locked', '--release', '--target', $Target)

    $platformName = if ($windowsTarget) { 'windows' } else { 'linux' }
    $architecture = $Target.Split('-')[0]
    $bundleName = "omni-morph-backend-$platformName-$architecture"
    $binaryName = if ($windowsTarget) { 'sculptor.exe' } else { 'sculptor' }
    $binary = Join-Path $repositoryRoot "target/$Target/release/$binaryName"
    if (-not (Test-Path -LiteralPath $binary -PathType Leaf) -or (Get-Item -LiteralPath $binary).Length -eq 0) {
        throw "Release executable is missing: $binary"
    }

    $bundleDirectory = Join-Path $repositoryRoot "dist/$bundleName"
    New-Item -ItemType Directory -Path $bundleDirectory -Force | Out-Null
    Copy-Item -LiteralPath $binary -Destination (Join-Path $bundleDirectory $binaryName) -Force
    Copy-Item -LiteralPath 'Config.example.toml', 'LICENSE' -Destination $bundleDirectory -Force
    Copy-Item -LiteralPath 'docs/DEPLOYMENT.md' -Destination (Join-Path $bundleDirectory 'DEPLOYMENT.md') -Force
    $binaryHash = (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash.ToLowerInvariant()
    "$binaryHash  $binaryName" | Set-Content -LiteralPath (Join-Path $bundleDirectory 'SHA256SUMS') -Encoding utf8NoBOM

    if ($windowsTarget) {
        $archive = Join-Path $repositoryRoot "dist/$bundleName.zip"
        $bundleFiles = Get-ChildItem -LiteralPath $bundleDirectory -File | Select-Object -ExpandProperty FullName
        Compress-Archive -LiteralPath $bundleFiles -DestinationPath $archive -Force 2>&1 |
            Tee-Object -FilePath build.log -Append
    } else {
        $archive = Join-Path $repositoryRoot "dist/$bundleName.tar.gz"
        & chmod 755 (Join-Path $bundleDirectory $binaryName)
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
        & tar -czf $archive -C $bundleDirectory . 2>&1 | Tee-Object -FilePath build.log -Append
        $buildExitCode = $LASTEXITCODE
        if ($buildExitCode -ne 0) {
            Get-Content -LiteralPath build.log -Tail 40
            exit $buildExitCode
        }
    }
    Get-Content -LiteralPath build.log -Tail 20
    $package = Get-Item -LiteralPath $archive
    $packageHash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Output "Package: $($package.Name) ($($package.Length) bytes)"
    Write-Output "SHA256: $packageHash"
} finally {
    Pop-Location
}
exit 0
