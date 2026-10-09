# irm https://raw.githubusercontent.com/ttokunaga-ja/aiClean/main/install.ps1 | iex
# BIN_DIR overrides the destination. AICLEAN_INSTALL_NO_PATH=1 is for isolated tests.
# Keep this scoped and never call exit: iex runs inside the user's PowerShell.
& {
    $ErrorActionPreference = 'Stop'
    if ($env:OS -ne 'Windows_NT' -or -not [Environment]::Is64BitOperatingSystem) { throw 'This installer requires Windows x64' }
    # Intended entry point; repository and release must be published first.
    $bin = if ($env:BIN_DIR) { $env:BIN_DIR } else { Join-Path $env:USERPROFILE '.local\bin' }
    if (-not [IO.Path]::IsPathRooted($bin)) { throw 'BIN_DIR は絶対パスにしてください' }
    $bin = [IO.Path]::GetFullPath($bin)
    if ($bin -ne [IO.Path]::GetPathRoot($bin)) { $bin = $bin.TrimEnd('\') }
    if ($bin.Contains(';') -or $bin.Contains("`r") -or $bin.Contains("`n")) { throw 'BIN_DIR に PATH の区切り文字や改行は使えません' }
    if ($env:AICLEAN_INSTALL_NO_PATH -ne '1' -and $bin.Contains('%')) { throw 'PATH を自動設定する場合、BIN_DIR に % は使えません' }
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    $temp = Join-Path ([IO.Path]::GetTempPath()) ('aiClean-install-' + [Guid]::NewGuid().ToString('N'))
    $stage = $null
    try {
        New-Item -ItemType Directory -Path $temp | Out-Null
        Add-Type -AssemblyName System.Net.Http
        $handler = [Net.Http.HttpClientHandler]::new()
        $handler.AllowAutoRedirect = $false
        $client = [Net.Http.HttpClient]::new($handler)
        $client.Timeout = [TimeSpan]::FromSeconds(120)
        $client.DefaultRequestHeaders.UserAgent.ParseAdd('aiClean-installer')
        function Fetch([string]$url, [string]$destination) {
            $deadline = [DateTime]::UtcNow.AddSeconds(120)
            $uri = [Uri]$url
            for ($redirect = 0; $redirect -lt 10; $redirect++) {
                if ($uri.Scheme -cne 'https') { throw 'HTTPS is required for every redirect' }
                $response = $client.GetAsync($uri, [Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
                try {
                    if ([int]$response.StatusCode -ge 300 -and [int]$response.StatusCode -lt 400) {
                        if (-not $response.Headers.Location) { throw 'Redirect has no Location' }
                        $uri = [Uri]::new($uri, $response.Headers.Location)
                        continue
                    }
                    $null = $response.EnsureSuccessStatusCode()
                    $stream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
                    $file = [IO.File]::Create($destination)
                    try {
                        $buffer = New-Object byte[] 65536
                        $total = 0
                        while ($true) {
                            $remaining = [int][Math]::Max(0, ($deadline - [DateTime]::UtcNow).TotalMilliseconds)
                            $read = $stream.ReadAsync($buffer, 0, $buffer.Length)
                            if ($remaining -eq 0 -or -not $read.Wait($remaining)) { throw 'Download timed out' }
                            $count = $read.GetAwaiter().GetResult()
                            if ($count -eq 0) { break }
                            $total += $count
                            if ($total -gt 134217728) { throw 'Download exceeds 128 MiB' }
                            $file.Write($buffer, 0, $count)
                        }
                    } finally { $file.Dispose(); $stream.Dispose() }
                    return
                } finally { $response.Dispose() }
            }
            throw 'Too many download redirects'
        }
        $metadata = Join-Path $temp 'release.json'
        try { Fetch 'https://api.github.com/repos/ttokunaga-ja/aiClean/releases/latest' $metadata }
        catch { throw "No published aiClean release could be retrieved. Build the local source with cargo build --release --locked. $($_.Exception.Message)" }
        $release = Get-Content -LiteralPath $metadata -Raw -Encoding UTF8 | ConvertFrom-Json
        $tag = [string]$release.tag_name
        if ($tag -cnotmatch '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$' -or $release.draft -or $release.prerelease) { throw 'Expected a published stable vX.Y.Z release' }
        $base = "https://github.com/ttokunaga-ja/aiClean/releases/download/$tag"
        $asset = 'aiClean-windows-x64.exe'
        $download = Join-Path $temp $asset
        $manifest = Join-Path $temp 'SHA256SUMS'
        Fetch "$base/$asset" $download
        Fetch "$base/SHA256SUMS" $manifest
        function Get-ExpectedHash([string]$manifest, [string]$asset) {
            $seen = @{}
            $expected = $null
            foreach ($line in Get-Content -LiteralPath $manifest -Encoding UTF8) {
                if ($line -cnotmatch '^([0-9a-fA-F]{64}) [ *]([^\s/\\]+)$') { throw 'Malformed SHA256SUMS entry' }
                $digest = $Matches[1]; $name = $Matches[2]
                if ($seen.ContainsKey($name)) { throw 'Duplicate SHA256SUMS entry' }
                $seen[$name] = $true
                if ($name -ceq $asset) { $expected = $digest }
            }
            if (-not $expected) { throw 'SHA256SUMS has no Windows x64 entry' }
            return $expected
        }
        $expected = Get-ExpectedHash $manifest $asset
        if ((Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash -ine $expected) { throw 'SHA-256 が一致しません' }
        New-Item -ItemType Directory -Path $bin -Force | Out-Null
        $exe = Join-Path $bin 'aiClean.exe'
        if (Test-Path -LiteralPath $exe) {
            $existing = Get-Item -LiteralPath $exe -Force
            if ($existing.PSIsContainer -or ($existing.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
                throw 'インストール先が通常ファイルではないため変更しません'
            }
        }
        $stage = Join-Path $bin ('.aiClean-install-' + [Guid]::NewGuid().ToString('N') + '.exe')
        [IO.File]::Copy($download, $stage, $false)
        function Confirm-Version([string]$stage, [string]$expectedVersion) {
            $info = [Diagnostics.ProcessStartInfo]::new()
            $info.FileName = $stage; $info.Arguments = '--version'
            $info.UseShellExecute = $false; $info.CreateNoWindow = $true
            $info.RedirectStandardOutput = $true; $info.RedirectStandardError = $true
            $process = [Diagnostics.Process]::new(); $process.StartInfo = $info
            try {
                $null = $process.Start()
                $stdout = $process.StandardOutput.ReadToEndAsync()
                $stderr = $process.StandardError.ReadToEndAsync()
                if (-not $process.WaitForExit(10000)) {
                    $process.Kill(); $process.WaitForExit()
                    throw 'Executable version check timed out; existing installation preserved'
                }
                if (-not $stdout.Wait(1000) -or -not $stderr.Wait(1000)) { throw 'Version output did not close; existing installation preserved' }
                $version = $stdout.GetAwaiter().GetResult().TrimEnd([char[]]"`r`n")
                if ($process.ExitCode -ne 0 -or $version -cne "aiClean $($expectedVersion)") { throw 'Executable version differs from release tag; existing installation preserved' }
            } finally { $process.Dispose() }
            return $version
        }
        $version = Confirm-Version $stage ($tag.Substring(1))
        function Install-Staged([string]$stage, [string]$exe, [string]$bin) {
            $backup = $null
            if (Test-Path -LiteralPath $exe) {
                # PowerShell 5.1 converts a null string argument to an empty path.
                $backup = Join-Path $bin ('.aiClean-backup-' + [Guid]::NewGuid().ToString('N') + '.exe')
                try { [IO.File]::Replace($stage, $exe, $backup) }
                catch {
                    if (Test-Path -LiteralPath $backup) { Write-Warning "既存の実行ファイルの退避先を保持しました: $backup" }
                    throw
                }
            } else { [IO.File]::Move($stage, $exe) }
            if ($backup -and (Test-Path -LiteralPath $backup)) {
                try { Remove-Item -LiteralPath $backup -Force }
                catch { Write-Warning "インストール済みですが、旧版の退避先を削除できませんでした: $backup" }
            }
        }
        Install-Staged $stage $exe $bin
        $stage = $null
        Write-Host "インストールしました: $exe ($version)"

        if ($env:AICLEAN_INSTALL_NO_PATH -ne '1') {
            $key = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
            try {
                $raw = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
                $present = @($raw -split ';' | Where-Object { [Environment]::ExpandEnvironmentVariables($_).Trim().Trim('"').TrimEnd('\') -ieq $bin })
                if ($present.Count -eq 0) {
                    $kind = if ($key.GetValueNames() -contains 'Path') { $key.GetValueKind('Path') } else { [Microsoft.Win32.RegistryValueKind]::ExpandString }
                    $separator = if ($raw.Length -eq 0 -or $raw.EndsWith(';')) { '' } else { ';' }
                    $key.SetValue('Path', ($raw + $separator + $bin), $kind)
                    # Notify Explorer without writing any unrelated environment variable.
                    if (-not ('AiCleanInstaller.EnvironmentNotification' -as [type])) {
                        Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
namespace AiCleanInstaller {
    public static class EnvironmentNotification {
        [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
        public static extern IntPtr SendMessageTimeout(IntPtr window, uint message, UIntPtr wParam, string lParam, uint flags, uint timeout, out UIntPtr result);
    }
}
'@
                    }
                    $result = [UIntPtr]::Zero
                    [void][AiCleanInstaller.EnvironmentNotification]::SendMessageTimeout([IntPtr]0xffff, 0x1a, [UIntPtr]::Zero, 'Environment', 2, 5000, [ref]$result)
                    Write-Host "ユーザー PATH に $bin を追加しました"
                }
            } finally { $key.Close() }
            $current = @($env:Path -split ';' | Where-Object { [Environment]::ExpandEnvironmentVariables($_).Trim().Trim('"').TrimEnd('\') -ieq $bin })
            if ($current.Count -eq 0) { $env:Path = $bin + ';' + $env:Path }
            Write-Host 'この PowerShell で aiClean を使えます。'
        }
    } finally {
        if ($client) { $client.Dispose() }
        if ($stage -and (Test-Path -LiteralPath $stage)) { Remove-Item -LiteralPath $stage -Force }
        if (Test-Path -LiteralPath $temp) { Remove-Item -LiteralPath $temp -Recurse -Force }
    }
}
