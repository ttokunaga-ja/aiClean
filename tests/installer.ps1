# Isolated native Windows fixtures: load only helper ASTs, never installer entry point.
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
# irm returns decoded text; a UTF-8 BOM becomes a command token under iex.
$installerBytes = [IO.File]::ReadAllBytes((Join-Path $root 'install.ps1'))
if ($installerBytes | Where-Object { $_ -gt 127 }) { throw 'Installer must be BOM-free ASCII for both iex and Windows PowerShell 5.1 file loading' }
$tokens = $null; $errors = $null
$ast = [Management.Automation.Language.Parser]::ParseFile((Join-Path $root 'install.ps1'), [ref]$tokens, [ref]$errors)
if ($errors.Count) { throw (($errors | ForEach-Object { 'Line ' + $_.Extent.StartLineNumber + ': ' + $_.Message }) -join "`n") }
foreach ($name in @('Get-ExpectedHash', 'Confirm-Version', 'Install-Staged')) {
    $definition = $ast.Find({param($node) $node -is [Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name}, $true)
    if (-not $definition) { throw "Missing helper $name" }
    Invoke-Expression $definition.Extent.Text
}
$temp = Join-Path ([IO.Path]::GetTempPath()) ('aiclean-installer-test-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $temp | Out-Null
function Assert([bool]$condition, [string]$detail) { if (-not $condition) { throw $detail } }
function Must-Fail([scriptblock]$action) {
    $failed = $false
    try { & $action | Out-Null } catch { $failed = $true }
    Assert $failed 'Expected fixture failure'
}
try {
    $manifest = Join-Path $temp 'SHA256SUMS'; $asset = 'aiClean-windows-x64.exe'
    $hash = 'a' * 64
    [IO.File]::WriteAllText($manifest, "$hash  $asset`n")
    Assert ((Get-ExpectedHash $manifest $asset) -ceq $hash) 'Valid checksum rejected'
    foreach ($body in @("$hash  $asset`n$hash  $asset`n", "$hash  $asset`nbroken`n", "abc  $asset`n", "$hash  ../$asset`n")) {
        [IO.File]::WriteAllText($manifest, $body)
        Must-Fail { Get-ExpectedHash $manifest $asset }
    }
    # .NET Framework compiler creates tiny native Windows version fixtures.
    $compiler = Join-Path $env:WINDIR 'Microsoft.NET\Framework64\v4.0.30319\csc.exe'
    Assert (Test-Path $compiler) 'Native Windows C# fixture compiler unavailable'
    $source = Join-Path $temp 'version.cs'; $candidate = Join-Path $temp 'candidate.exe'
    foreach ($version in @('0.2.0', '0.2.1')) {
        [IO.File]::WriteAllText($source, ('class Fixture { static void Main() { System.Console.WriteLine("aiClean ' + $version + '"); } }'))
        & $compiler /nologo "/out:$candidate" $source
        Assert ($LASTEXITCODE -eq 0) 'Fixture compiler failed'
        if ($version -eq '0.2.0') { Assert ((Confirm-Version $candidate '0.2.0') -ceq 'aiClean 0.2.0') 'Valid staged version rejected' }
        else { Must-Fail { Confirm-Version $candidate '0.2.0' } }
    }
    $installed = Join-Path $temp 'aiClean.exe'; $stage = Join-Path $temp 'stage.exe'
    [IO.File]::WriteAllText($installed, 'original'); [IO.File]::WriteAllText($stage, 'replacement')
    $lock = [IO.File]::Open($installed, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::None)
    try { Must-Fail { Install-Staged $stage $installed $temp } } finally { $lock.Dispose() }
    Assert ([IO.File]::ReadAllText($installed) -ceq 'original') 'Failed replace modified installation'
    Install-Staged $stage $installed $temp
    Assert ([IO.File]::ReadAllText($installed) -ceq 'replacement') 'Atomic replacement failed'
    Write-Host 'Windows installer fixtures passed (manifest, version, atomic failure preservation)'
} finally { Remove-Item -LiteralPath $temp -Recurse -Force }
