# -----------------------------------------------------
# Sentinel MCP Client Setup (Windows)
# Automatically configure AI tools to connect to your
# Sentinel cameras via the Model Context Protocol.
# -----------------------------------------------------

param(
    [Parameter(Position=0)]
    [string]$ApiKey,
    [Parameter(Position=1)]
    [string]$ServerUrl
)

# Normalize: ensure trailing slash on the server URL.  FastAPI mounts
# the MCP app at "/mcp" with internal path="/", so a POST to "/mcp"
# (no slash) emits a 307 redirect to "/mcp/".  Strict-HTTPS clients
# like mcp-remote can't follow that redirect cleanly when the proxy
# emits the redirect with http:// scheme (Fly's edge upgrades it
# transparently, the client doesn't), so we hand them the
# redirect-free URL up front.  The server-side --forwarded-allow-ips
# fix in the Dockerfile makes the redirect itself correct, but having
# both means even an old-server new-client mismatch still works.
if ($ServerUrl -and -not $ServerUrl.EndsWith('/')) {
    $ServerUrl = $ServerUrl + '/'
}

if (-not $ApiKey -or -not $ServerUrl) {
    Write-Host ""
    Write-Host "  Error: Missing arguments" -ForegroundColor Red
    Write-Host ""
    Write-Host "  Usage (local):  .\mcp-setup.ps1 <api_key> <server_url>"
    Write-Host "  Usage (remote): & ([scriptblock]::Create((irm <url>/mcp-setup.ps1))) <api_key> <server_url>"
    Write-Host ""
    Write-Host "  Get your command from the Sentinel MCP dashboard:"
    Write-Host "  https://opensentry-command.fly.dev/mcp" -ForegroundColor Cyan
    Write-Host ""
    exit 1
}

# -- Header --------------------------------------------

Write-Host ""
Write-Host "  Sentinel MCP Setup" -ForegroundColor Green
Write-Host "  Configure AI tools to connect to your cameras" -ForegroundColor DarkGray
Write-Host ""

# -- Helpers -------------------------------------------

# Per-client outcome tracking.  Used to compute the final summary and exit
# code — without this the script previously printed "Setup Complete" in
# green even when every selected client was skipped or failed, which lied
# to the user about what actually happened.
#
#   ConfiguredClients : write succeeded, AI client now wired up
#   SkippedClients    : refused because the client process was running
#   FailedClients     : read/parse/write error — array of @{Name; Reason}
$script:ConfiguredClients = @()
$script:SkippedClients = @()
$script:FailedClients = @()

# Process names to check per client. Claude Desktop ships a bundled Claude Code
# experience so the same `Claude.exe` process touches `.claude.json` too --
# detecting Claude.exe covers both client rows. A separate `claude` CLI (npm
# install) runs as a node process and we can't reliably fingerprint it, so the
# "Quit Claude Code before continuing" warning still has to carry that case.
$script:ClientProcesses = @{
    'Claude Code'    = @('Claude')
    'Claude Desktop' = @('Claude')
    'Cursor'         = @('Cursor')
    'Windsurf'       = @('Windsurf')
}

# Returns the PID of a running matching process, or $null. Returns $null when
# the env var SOURCEBOX_SENTRY_MCP_ALLOW_RUNNING=1 is set -- tests set that so they
# can run even while a real Claude Desktop is up on the dev machine.
function Test-ClientRunning {
    param([string]$ClientName)
    if ($env:SOURCEBOX_SENTRY_MCP_ALLOW_RUNNING -eq "1") { return $null }
    $names = $script:ClientProcesses[$ClientName]
    if (-not $names) { return $null }
    foreach ($n in $names) {
        $proc = Get-Process -Name $n -ErrorAction SilentlyContinue
        if ($proc) { return $proc[0].Id }
    }
    return $null
}

# Recursively convert ConvertFrom-Json output (PSCustomObject / arrays /
# primitives) into hashtables/arrays we can mutate. Works on PowerShell 5.1+ --
# we deliberately DON'T use ConvertFrom-Json -AsHashtable because that flag was
# only added in PowerShell 6.0 and silently throws on 5.1.
function ConvertTo-OscHashtable($obj) {
    if ($null -eq $obj) { return $null }
    if ($obj -is [System.Collections.IDictionary]) {
        $h = [ordered]@{}
        foreach ($k in $obj.Keys) { $h[$k] = ConvertTo-OscHashtable $obj[$k] }
        return $h
    }
    if ($obj -is [pscustomobject]) {
        $h = [ordered]@{}
        foreach ($p in $obj.PSObject.Properties) {
            $h[$p.Name] = ConvertTo-OscHashtable $p.Value
        }
        return $h
    }
    # Arrays -- but NOT strings, which PS treats as char-iterable.
    #
    # CRITICAL: use [System.Collections.Generic.List[object]] here, NOT
    # `@(...)`.  In PowerShell 5.1, an `object[]` array (which is what
    # `@(...)` produces) inside an OrderedDictionary triggers a
    # ConvertTo-Json bug where every string element gets serialized as
    # `{"Length": N}` -- ConvertTo-Json walks each element looking for
    # properties to emit and finds .Length on the string class.  A
    # generic list keeps each item's runtime type stable through
    # serialization.
    #
    # The leading comma (`return ,$list`) prevents PowerShell from
    # unwrapping a single-element list back into a scalar at the call
    # site, which would lose the array-ness for `"args": ["one-thing"]`.
    #
    # User report that surfaced this: running the auto-setup script
    # against an existing claude_desktop_config.json with an MCP_DOCKER
    # entry mangled `"args": ["mcp", "gateway", "run", ...]` into
    # `"args": [{"Length":3}, {"Length":7}, ...]`, breaking Claude
    # Desktop on next launch.
    if ($obj -is [System.Collections.IEnumerable] -and -not ($obj -is [string])) {
        $list = [System.Collections.Generic.List[object]]::new()
        foreach ($item in $obj) {
            $list.Add((ConvertTo-OscHashtable $item))
        }
        return ,$list
    }
    return $obj
}

# -- Detect MCP Clients --------------------------------

$clients = @()

# Claude Code
$claudeCodePath = Join-Path $env:USERPROFILE ".claude.json"
$claudeCodeDetected = (Test-Path $claudeCodePath) -or ($null -ne (Get-Command claude -ErrorAction SilentlyContinue))
$clients += @{
    Name = "Claude Code"
    Path = $claudeCodePath
    Detected = $claudeCodeDetected
}

# Claude Desktop
$claudeDesktopPath = Join-Path $env:APPDATA "Claude\claude_desktop_config.json"
$claudeDesktopDetected = Test-Path (Split-Path $claudeDesktopPath -Parent)
$clients += @{
    Name = "Claude Desktop"
    Path = $claudeDesktopPath
    Detected = $claudeDesktopDetected
}

# Cursor
$cursorPath = Join-Path $env:USERPROFILE ".cursor\mcp.json"
$cursorDetected = Test-Path (Split-Path $cursorPath -Parent)
$clients += @{
    Name = "Cursor"
    Path = $cursorPath
    Detected = $cursorDetected
}

# Windsurf
$windsurfPath = Join-Path $env:USERPROFILE ".codeium\windsurf\mcp_config.json"
$windsurfDetected = Test-Path (Split-Path $windsurfPath -Parent)
$clients += @{
    Name = "Windsurf"
    Path = $windsurfPath
    Detected = $windsurfDetected
}

# -- Display Detected Clients -------------------------

Write-Host "  MCP Clients:" -ForegroundColor White
Write-Host ""

$detectedCount = 0
for ($i = 0; $i -lt $clients.Count; $i++) {
    $c = $clients[$i]
    $num = $i + 1
    if ($c.Detected) {
        Write-Host "    [$num] " -ForegroundColor Green -NoNewline
        Write-Host ([char]0x25CF) -ForegroundColor Green -NoNewline
        Write-Host " $($c.Name)" -ForegroundColor White
        Write-Host "        $($c.Path)" -ForegroundColor DarkGray
        $detectedCount++
    } else {
        Write-Host "    [$num] " -ForegroundColor DarkGray -NoNewline
        Write-Host ([char]0x25CB) -ForegroundColor DarkGray -NoNewline
        Write-Host " $($c.Name)" -ForegroundColor DarkGray
        Write-Host "        $($c.Path) (not found)" -ForegroundColor DarkGray
    }
}

Write-Host ""

if ($detectedCount -eq 0) {
    Write-Host "  No MCP clients detected." -ForegroundColor Yellow
    Write-Host "  You can still configure a client by entering its number." -ForegroundColor DarkGray
    Write-Host ""
}

# -- Warning: quit target apps ------------------------

Write-Host "  Important:" -ForegroundColor Yellow
Write-Host "  Quit Claude Code / Claude Desktop / Cursor / Windsurf before continuing." -ForegroundColor DarkGray
Write-Host "  Running clients may overwrite config changes while the setup is writing." -ForegroundColor DarkGray
Write-Host ""

# -- Prompt for Selection ------------------------------

Write-Host "  Which clients would you like to configure?" -ForegroundColor White
Write-Host "  Enter numbers separated by commas (e.g. 1,3), 'all' for all detected, or 'q' to quit" -ForegroundColor DarkGray
Write-Host ""
$selection = Read-Host "  >"

if ($selection -eq "q" -or $selection -eq "Q") {
    Write-Host ""
    Write-Host "  Setup cancelled." -ForegroundColor DarkGray
    Write-Host ""
    exit 0
}

# Parse selection
$selected = @()
if ($selection -eq "all" -or $selection -eq "ALL") {
    for ($i = 0; $i -lt $clients.Count; $i++) {
        if ($clients[$i].Detected) {
            $selected += $i
        }
    }
    if ($selected.Count -eq 0) {
        Write-Host ""
        Write-Host "  No detected clients to configure." -ForegroundColor Yellow
        Write-Host ""
        exit 0
    }
} else {
    $nums = $selection -split "," | ForEach-Object { $_.Trim() }
    foreach ($num in $nums) {
        $idx = [int]$num - 1
        if ($idx -ge 0 -and $idx -lt $clients.Count) {
            $selected += $idx
        } else {
            Write-Host "  Skipping invalid selection: $num" -ForegroundColor Yellow
        }
    }
}

if ($selected.Count -eq 0) {
    Write-Host ""
    Write-Host "  No valid selections. Exiting." -ForegroundColor Yellow
    Write-Host ""
    exit 0
}

Write-Host ""

# -- Configure Selected Clients ------------------------

function Configure-Client {
    param(
        [string]$Name,
        [string]$ConfigPath
    )

    Write-Host "  Configuring $Name..." -ForegroundColor Blue

    # Refuse to touch the config if the target client is currently running --
    # its own file-watcher will clobber our write with stale in-memory state.
    # That's exactly what happened in the original bug: a correctly-written
    # config got stomped back to defaults (and our mcpServers entry erased)
    # within a second of the write.
    $runningPid = Test-ClientRunning -ClientName $Name
    if ($runningPid) {
        Write-Host "    $Name is currently running (pid $runningPid)." -ForegroundColor Yellow
        Write-Host "    Skipping -- quit $Name completely and re-run this script." -ForegroundColor Yellow
        Write-Host "    Your config was NOT modified." -ForegroundColor DarkGray
        Write-Host ""
        $script:SkippedClients += $Name
        return
    }

    # Create parent directory if needed
    $dir = Split-Path $ConfigPath -Parent
    if (-not (Test-Path $dir)) {
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
        Write-Host "    Created directory: $dir" -ForegroundColor DarkGray
    }

    # Read + parse existing config. If the file exists and is non-empty but
    # unparseable, we ABORT rather than clobber -- losing an existing
    # .claude.json full of session state is catastrophic.
    $config = $null
    if (Test-Path $ConfigPath) {
        $content = $null
        try {
            $content = Get-Content $ConfigPath -Raw -ErrorAction Stop
        } catch {
            Write-Host "    Failed to read $ConfigPath : $_" -ForegroundColor Red
            Write-Host "    Skipping $Name -- your file was not modified." -ForegroundColor Yellow
            Write-Host ""
            $script:FailedClients += @{ Name = $Name; Reason = "could not read $ConfigPath" }
            return
        }

        if ($content -and $content.Trim()) {
            try {
                $parsed = $content | ConvertFrom-Json -ErrorAction Stop
                $config = ConvertTo-OscHashtable $parsed
            } catch {
                Write-Host "    Could not parse $ConfigPath as JSON." -ForegroundColor Red
                Write-Host "    Error: $($_.Exception.Message)" -ForegroundColor DarkGray
                Write-Host "    Skipping $Name to avoid overwriting your existing data." -ForegroundColor Yellow
                Write-Host "    Fix the file manually (or delete it) and re-run." -ForegroundColor DarkGray
                Write-Host ""
                $script:FailedClients += @{ Name = $Name; Reason = "existing config is not valid JSON" }
                return
            }
        } else {
            $config = [ordered]@{}
        }
    } else {
        $config = [ordered]@{}
    }

    # Ensure mcpServers exists (preserving any other entries already there).
    if (-not $config.Contains("mcpServers") -or $null -eq $config["mcpServers"]) {
        $config["mcpServers"] = [ordered]@{}
    }

    # Build the right config shape for this specific client.  Each MCP
    # client speaks a slightly different config schema and they are
    # NOT interchangeable -- writing the wrong shape silently produces
    # an entry the client refuses to load.
    #
    # Original bug: the script wrote `{type:"http", url, headers}` to
    # every client.  That shape is correct for Claude Code (which
    # speaks streamable HTTP MCP natively) but Claude Desktop's MCP
    # loader rejects it with "not valid MCP server configurations and
    # were skipped: opensentry".  Claude Desktop only loads stdio
    # servers; remote HTTP MCP servers need to be wrapped with the
    # `mcp-remote` adapter (npx package) which fronts the HTTP server
    # as a local stdio process.  Cursor accepts `{url, headers}` (no
    # `type` field).  Windsurf uses `serverUrl` instead of `url`.
    $opensentryConfig = switch ($Name) {
        'Claude Code' {
            [ordered]@{
                type = "http"
                url = $ServerUrl
                headers = [ordered]@{ Authorization = "Bearer $ApiKey" }
            }
        }
        'Claude Desktop' {
            # mcp-remote (npm: mcp-remote) wraps a remote HTTP MCP
            # server as a local stdio process.  Requires Node.js on
            # PATH; we warn separately if it's missing.
            #
            # Why `cmd /c npx ...` instead of just `command: "npx"`:
            # On Windows, npm installs npx as `npx.cmd` (a batch file).
            # When Claude Desktop sees command="npx", it auto-resolves
            # via PATHEXT to `C:\Program Files\nodejs\npx.cmd`, then
            # wraps with `cmd.exe /C` to execute the .cmd file -- but
            # it passes the resolved path UNQUOTED, so cmd.exe sees
            # `C:\Program` as the command and the rest as args, with
            # the lovely error
            #   'C:\Program' is not recognized as an internal or
            #   external command, operable program or batch file.
            # By using `cmd` (a .exe, no wrapping) and `/c npx ...` as
            # args, we let cmd.exe do its own PATHEXT resolution
            # natively without the quoting bug.  Standard pattern for
            # npx-based MCP servers on Windows.
            [ordered]@{
                command = "cmd"
                args = @("/c", "npx", "-y", "mcp-remote", $ServerUrl, "--header", "Authorization:Bearer $ApiKey")
            }
        }
        'Cursor' {
            [ordered]@{
                url = $ServerUrl
                headers = [ordered]@{ Authorization = "Bearer $ApiKey" }
            }
        }
        'Windsurf' {
            [ordered]@{
                serverUrl = $ServerUrl
                headers = [ordered]@{ Authorization = "Bearer $ApiKey" }
            }
        }
        default {
            [ordered]@{
                type = "http"
                url = $ServerUrl
                headers = [ordered]@{ Authorization = "Bearer $ApiKey" }
            }
        }
    }

    $config["mcpServers"]["opensentry"] = $opensentryConfig

    # Claude Desktop's mcp-remote adapter needs Node.js.  Warn early
    # rather than letting the user discover it via a cryptic
    # "npx not found" error inside Claude Desktop after restart.
    if ($Name -eq 'Claude Desktop') {
        $nodeAvailable = $null -ne (Get-Command node -ErrorAction SilentlyContinue)
        if (-not $nodeAvailable) {
            Write-Host "    Note: Node.js was not found on PATH." -ForegroundColor Yellow
            Write-Host "    Claude Desktop uses mcp-remote (an npx package) to talk to" -ForegroundColor DarkGray
            Write-Host "    Sentinel's HTTP MCP server.  Install Node.js from" -ForegroundColor DarkGray
            Write-Host "    https://nodejs.org/ then restart Claude Desktop." -ForegroundColor DarkGray
        }
    }

    # Self-test: serialize and re-parse before writing.  If
    # ConvertFrom-Json -> ConvertTo-OscHashtable -> ConvertTo-Json
    # corrupts shapes (the canonical case being string elements in
    # an `args` array becoming `{"Length": N}` objects under PS 5.1),
    # abort rather than write the corrupted JSON to disk.  Catches
    # the regression that broke a user's Claude Desktop config when
    # the script silently mangled their MCP_DOCKER args array.
    try {
        $candidateJson = $config | ConvertTo-Json -Depth 100
        $candidateParsed = $candidateJson | ConvertFrom-Json -ErrorAction Stop

        if ($null -ne $candidateParsed.mcpServers) {
            foreach ($srv in $candidateParsed.mcpServers.PSObject.Properties) {
                # IMPORTANT: do NOT name this `$name` -- PowerShell variables
                # are case-insensitive, and `$name` is the same slot as the
                # outer function parameter `$Name`.  Reassigning it inside
                # this loop quietly clobbers the client name that the
                # summary block reads later, so the user sees
                # "Configured: * opensentry" instead of "* Claude Desktop".
                $srvName = $srv.Name
                $val = $srv.Value
                if ($null -ne $val -and $null -ne $val.args) {
                    # Coerce to array so .Count works whether args is a single
                    # value or a real array.
                    $argsArr = @($val.args)
                    for ($i = 0; $i -lt $argsArr.Count; $i++) {
                        $a = $argsArr[$i]
                        # The canonical corruption: a string became
                        # {Length: N}.  Detect by checking for that exact
                        # property without the original string value being
                        # recoverable.
                        if ($a -is [pscustomobject] -and
                            $a.PSObject.Properties.Name -contains 'Length' -and
                            $a.PSObject.Properties.Count -eq 1) {
                            throw "args[$i] of '$srvName' was corrupted by the JSON roundtrip (string -> {Length: $($a.Length)} object).  Aborting write to protect existing config."
                        }
                    }
                }
            }
        }
    } catch {
        Write-Host "    Self-test failed for $Name : $_" -ForegroundColor Red
        Write-Host "    Your config was NOT modified." -ForegroundColor DarkGray
        Write-Host ""
        $script:FailedClients += @{ Name = $Name; Reason = "self-test detected JSON roundtrip corruption: $($_.Exception.Message)" }
        return
    }

    # ALWAYS back up the file before we overwrite it -- even when parsing
    # succeeded, because a disk write can fail halfway through.
    if (Test-Path $ConfigPath) {
        $backup = "$ConfigPath.bak"
        try {
            Copy-Item $ConfigPath $backup -Force
            Write-Host "    Backed up existing config to $backup" -ForegroundColor DarkGray
        } catch {
            Write-Host "    Warning: could not create backup at $backup : $_" -ForegroundColor Yellow
        }
    }

    # Write back. Use -Depth 100 -- Claude Code configs contain deeply nested
    # project state that truncates silently at the default depth of 2.
    #
    # Write UTF-8 *without* a BOM: PowerShell 5.1's `Set-Content -Encoding UTF8`
    # prepends a byte-order mark, and Claude Desktop's JSON parser rejects it
    # with "Unexpected token ''... is not valid JSON". Using .NET directly
    # behaves the same on PS 5.1 and 7+.
    try {
        # Reuse $candidateJson from the self-test above -- avoids re-running
        # ConvertTo-Json (and its pretty-printing pass) twice for the same
        # config object.
        $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
        [System.IO.File]::WriteAllText($ConfigPath, $candidateJson, $utf8NoBom)
        Write-Host "    Done -> $ConfigPath" -ForegroundColor Green
        $script:ConfiguredClients += $Name
    } catch {
        Write-Host "    Failed to configure $Name : $_" -ForegroundColor Red
        Write-Host "    Your backup is at $ConfigPath.bak" -ForegroundColor DarkGray
        $script:FailedClients += @{ Name = $Name; Reason = "write failed: $($_.Exception.Message)" }
    }

    Write-Host ""
}

foreach ($idx in $selected) {
    $c = $clients[$idx]
    Configure-Client -Name $c.Name -ConfigPath $c.Path
}

# Wait for the user before exiting so the terminal window doesn't slam
# shut on them mid-summary.  Only relevant when the script was launched
# into a fresh PowerShell window (double-click, "Run" dialog, or a
# parent process that runs powershell.exe and exits) -- in those cases
# the host closes as soon as the script returns.  Skip when:
#   - SOURCEBOX_SENTRY_MCP_NO_PAUSE=1 (CI / scripted callers)
#   - We're not in an interactive console (piped stdin, no UI)
function Wait-ForExitKey {
    if ($env:SOURCEBOX_SENTRY_MCP_NO_PAUSE -eq '1') { return }
    if (-not [Environment]::UserInteractive) { return }
    if ($Host.Name -ne 'ConsoleHost') { return }

    Write-Host ""
    Write-Host "  Press Enter to close..." -ForegroundColor DarkGray
    try { $null = Read-Host } catch { }
}

# -- Summary -------------------------------------------
#
# Three possible end-states; pick the right banner + exit code for each.
# Previously the script always said "Setup Complete" in green even when
# every selected client got skipped (because the client was running) --
# the user thought their AI tools were wired up when nothing changed.

$configuredCount = $script:ConfiguredClients.Count
$skippedCount = $script:SkippedClients.Count
$failedCount = $script:FailedClients.Count

# Per-bucket details first so the user sees what happened before the banner.
if ($script:ConfiguredClients.Count -gt 0) {
    Write-Host "  Configured:" -ForegroundColor Green
    foreach ($c in $script:ConfiguredClients) {
        Write-Host "    * $c" -ForegroundColor Green
    }
    Write-Host ""
}

if ($script:SkippedClients.Count -gt 0) {
    Write-Host "  Skipped (still running):" -ForegroundColor Yellow
    foreach ($s in $script:SkippedClients) {
        Write-Host "    * $s" -ForegroundColor Yellow
    }
    Write-Host ""
    Write-Host "  Quit the above and re-run this script to configure them." -ForegroundColor DarkGray
    Write-Host ""
}

if ($script:FailedClients.Count -gt 0) {
    Write-Host "  Failed:" -ForegroundColor Red
    foreach ($f in $script:FailedClients) {
        Write-Host "    * $($f.Name) -- $($f.Reason)" -ForegroundColor Red
    }
    Write-Host ""
}

# Final banner + exit code.
if ($configuredCount -eq 0) {
    # Nothing was configured.  Could be 100% skipped, 100% failed, or a
    # mix -- either way the script did not do its job and the exit code
    # has to reflect that for any non-interactive caller.
    Write-Host "  Setup did NOT complete" -ForegroundColor Red
    Write-Host ""
    Write-Host "  No clients were configured." -ForegroundColor DarkGray
    if ($skippedCount -gt 0) {
        Write-Host "  Quit the running clients listed above and re-run." -ForegroundColor DarkGray
    }
    Write-Host ""
    Write-Host "  Manage your MCP keys at:" -ForegroundColor DarkGray
    $dashUrl = $ServerUrl -replace "/mcp$", "/mcp"
    Write-Host "  $dashUrl" -ForegroundColor Cyan
    Write-Host ""
    Wait-ForExitKey
    exit 1
} elseif ($skippedCount -gt 0 -or $failedCount -gt 0) {
    # At least one configured, but at least one didn't.  Don't claim
    # green-checkmark success; tell the user clearly that some clients
    # need a re-run.
    Write-Host "  Setup completed with warnings" -ForegroundColor Yellow
    Write-Host ""
    Write-Host "  $configuredCount of $($configuredCount + $skippedCount + $failedCount) clients configured." -ForegroundColor DarkGray
    Write-Host "  Restart the configured clients so they pick up the new MCP server." -ForegroundColor DarkGray
    Write-Host ""
    Write-Host "  Manage your MCP keys at:" -ForegroundColor DarkGray
    $dashUrl = $ServerUrl -replace "/mcp$", "/mcp"
    Write-Host "  $dashUrl" -ForegroundColor Cyan
    Write-Host ""
    # Exit 0 -- the script did configure something, the user just needs
    # a follow-up run for the rest.  Use the warning banner to make that
    # visible without breaking shell pipelines that key off exit codes.
    Wait-ForExitKey
    exit 0
} else {
    # Clean success.
    Write-Host "  Setup Complete" -ForegroundColor Green
    Write-Host ""
    Write-Host "  Your AI tools can now access your Sentinel cameras." -ForegroundColor DarkGray
    Write-Host "  Restart the clients you configured so they pick up the new MCP server." -ForegroundColor DarkGray
    Write-Host "  Try asking: `"List my cameras`" or `"Show me what the front door sees`"" -ForegroundColor DarkGray
    Write-Host ""
    Write-Host "  Manage your MCP keys at:" -ForegroundColor DarkGray
    $dashUrl = $ServerUrl -replace "/mcp$", "/mcp"
    Write-Host "  $dashUrl" -ForegroundColor Cyan
    Write-Host ""
    Wait-ForExitKey
    exit 0
}
