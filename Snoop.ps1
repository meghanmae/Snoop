<#
.SYNOPSIS
    Snoop - a minimal stand-in for Appfire Flow's activity dashboard.

.DESCRIPTION
    Pulls code commits, merge commits, PR activity, PR comments and work-item
    (ticket) activity out of a single Azure DevOps project, buckets every event
    by person and by day, and writes a self-contained dark-mode HTML dashboard.

    No database, no login, no server. Run it, look at it, close it.

.EXAMPLE
    .\Snoop.ps1
    Reads snoop.config.json and opens the dashboard. On the very first run,
    writes a starter config file next to the script.

.EXAMPLE
    .\Snoop.ps1 -Organization contoso -Project Apollo -Repository web,api -DaysBack 30
    Anything passed on the command line overrides the config file for that run.

.EXAMPLE
    .\Snoop.ps1 -People 'Alvarez-CONTRACTOR, Robin','Dunn-CONTRACTOR, Riley'
    Chart just those two, whatever the config file says.

.EXAMPLE
    .\Snoop.ps1 -ConfigFile .\client.config.json
    Keep a separate config per client/repo and switch between them.

.NOTES
    Auth: uses your existing `az login` by default. If the target org lives in a
    tenant your az login does not cover, set a PAT instead:
        $env:ADO_PAT = '<personal access token>'
    The PAT needs read access to Code, Pull Requests and Work Items.
#>
[CmdletBinding()]
param(
    [string]   $ConfigFile,
    [string]   $Organization,
    [string]   $Project,
    [string[]] $Repository,
    [string[]] $People,
    [int]      $DaysBack,
    [string]   $WorkItemOrganization,
    [string[]] $WorkItemProject,
    [string]   $OutFile,
    [switch]   $IncludeWeekends,
    [switch]   $AutoDiscoverPeople,
    [switch]   $DefaultBranchOnly,
    [switch]   $SkipWorkItems,
    [switch]   $SkipPrComments,
    [switch]   $NoOpen
)

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

# ---------------------------------------------------------------------------
# Configuration
#
# Everything lives in snoop.config.json next to this script. Command line
# arguments override it for a single run. An empty "people" list means
# "show me whoever is actually active here", capped at maxDiscoveredPeople.
# ---------------------------------------------------------------------------

$DefaultConfig = [ordered]@{
    organization        = 'contoso'
    project             = 'Apollo'
    repositories        = @('Apollo')
    daysBack            = 14
    people              = @()
    aliases             = [ordered]@{}
    # Tickets do not always live with the code. Blank fields mean "same as the
    # repo above"; fill them in when the boards are in another project or org.
    workItems           = [ordered]@{
        organization = ''
        projects     = @()
        areaPaths    = @()
    }
    maxDiscoveredPeople = 20
    prLookbackDays      = 90
    includeAllBranches  = $true
    maxBranches         = 50
    includeWeekends     = $false
}

if (-not $ConfigFile) { $ConfigFile = Join-Path $PSScriptRoot 'snoop.config.json' }

if (-not (Test-Path -LiteralPath $ConfigFile)) {
    ($DefaultConfig | ConvertTo-Json -Depth 5) | Set-Content -LiteralPath $ConfigFile -Encoding UTF8
    Write-Host "Created starter config: $ConfigFile" -ForegroundColor Yellow
}

try { $FileConfig = Get-Content -Raw -LiteralPath $ConfigFile | ConvertFrom-Json }
catch { throw "Could not parse $ConfigFile - $($_.Exception.Message)" }

function Get-Setting {
    <# Command line wins, then the config file, then the built-in default. #>
    param([string] $Key, $Override)

    $supplied = $null -ne $Override -and
                -not ($Override -is [string] -and [string]::IsNullOrEmpty($Override)) -and
                -not ($Override -is [int]    -and $Override -eq 0) -and
                -not ($Override -is [array]  -and $Override.Count -eq 0)
    if ($supplied) { return $Override }

    $property = $FileConfig.PSObject.Properties[$Key]
    if ($property -and $null -ne $property.Value) { return $property.Value }

    return $DefaultConfig[$Key]
}

$Organization   = Get-Setting 'organization'   $Organization
$Project        = Get-Setting 'project'        $Project
$Repository     = @(Get-Setting 'repositories' $Repository)
$DaysBack       = [int](Get-Setting 'daysBack' $DaysBack)
$People         = @(Get-Setting 'people'       $People)
$PrLookbackDays = [int](Get-Setting 'prLookbackDays' $null)
$MaxDiscovered  = [int](Get-Setting 'maxDiscoveredPeople' $null)
$MaxBranches    = [int](Get-Setting 'maxBranches' $null)

if (-not $IncludeWeekends -and (Get-Setting 'includeWeekends' $null)) { $IncludeWeekends = [switch]$true }

# Feature branches are where in-progress work lives. Scanning only the default
# branch hides anyone whose work has not merged yet - or whose PRs get squashed,
# since a squash rewrites the author onto whoever completed the PR.
$AllBranches = -not $DefaultBranchOnly
if ($AllBranches) {
    $configured = Get-Setting 'includeAllBranches' $null
    if ($null -ne $configured) { $AllBranches = [bool]$configured }
}

# Tickets can live in another project, or another org entirely.
function Get-SubSetting {
    param($Parent, [string] $Key)
    if ($null -eq $Parent) { return $null }
    $property = $Parent.PSObject.Properties[$Key]
    if ($property) { return $property.Value }
    return $null
}

$workItemConfig = Get-Setting 'workItems' $null

$WorkItemOrg = $WorkItemOrganization
if (-not $WorkItemOrg) { $WorkItemOrg = Get-SubSetting $workItemConfig 'organization' }
if (-not $WorkItemOrg) { $WorkItemOrg = $Organization }

$WorkItemProjects = @($WorkItemProject)
if ($WorkItemProjects.Count -eq 0) { $WorkItemProjects = @(Get-SubSetting $workItemConfig 'projects' | Where-Object { $_ }) }
if ($WorkItemProjects.Count -eq 0) { $WorkItemProjects = @($Project) }

$WorkItemAreaPaths = @(Get-SubSetting $workItemConfig 'areaPaths' | Where-Object { $_ })

# Map an awkward git handle onto one of the roster names, for the cases where
# the two share no name tokens at all. e.g. "jhollister" -> "Hollister, Jordan"
$Aliases = @{}
if ($FileConfig.PSObject.Properties['aliases'] -and $FileConfig.aliases) {
    foreach ($property in $FileConfig.aliases.PSObject.Properties) { $Aliases[$property.Name] = $property.Value }
}

# No roster at all means "discover whoever is here" rather than "show nothing".
$DiscoverMode = $AutoDiscoverPeople -or $People.Count -eq 0

# ---------------------------------------------------------------------------
# Plumbing
# ---------------------------------------------------------------------------

$AdoResourceId = '499b84ac-1321-427f-aa17-267ca6975798'   # Azure DevOps

function Get-AdoHeaders {
    if ($env:ADO_PAT) {
        Write-Host 'Auth: personal access token ($env:ADO_PAT)' -ForegroundColor DarkGray
        $raw = [Text.Encoding]::ASCII.GetBytes(":$($env:ADO_PAT)")
        return @{
            Authorization  = 'Basic ' + [Convert]::ToBase64String($raw)
            'Content-Type' = 'application/json'
        }
    }

    $token = az account get-access-token --resource $AdoResourceId --query accessToken -o tsv 2>$null
    if ([string]::IsNullOrWhiteSpace($token)) {
        throw "Could not get an Azure DevOps token. Run 'az login', or if the org is in another tenant set `$env:ADO_PAT to a personal access token."
    }
    $who = az account show --query user.name -o tsv 2>$null
    Write-Host "Auth: az login ($who)" -ForegroundColor DarkGray
    return @{ Authorization = "Bearer $token"; 'Content-Type' = 'application/json' }
}

$Headers = Get-AdoHeaders
$ApiBase = "https://dev.azure.com/$Organization/$([uri]::EscapeDataString($Project))/_apis"

function Invoke-Ado {
    param(
        [Parameter(Mandatory)] [string] $Uri,
        [string] $Method = 'GET',
        $Body
    )
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        try {
            if ($Body) {
                return Invoke-RestMethod -Uri $Uri -Headers $Headers -Method $Method -Body $Body
            }
            return Invoke-RestMethod -Uri $Uri -Headers $Headers -Method $Method
        }
        catch {
            $status = $null
            if ($_.Exception.Response) { $status = [int]$_.Exception.Response.StatusCode }
            if ($attempt -lt 3 -and ($status -eq 429 -or $status -ge 500 -or $null -eq $status)) {
                Start-Sleep -Seconds ($attempt * 2)
                continue
            }
            if ($status -eq 401 -or $status -eq 203) {
                throw "Azure DevOps rejected the credentials for '$Organization'. If that org is in another tenant, set `$env:ADO_PAT and re-run."
            }
            throw "$Method $Uri failed: $($_.Exception.Message)"
        }
    }
}

# --- identity matching ------------------------------------------------------

function Get-NameKeys {
    <# Reduce a name/email to canonical keys so that "Alvarez-CONTRACTOR, Robin",
       "Robin Alvarez" and "robin.alvarez@x.com" all collapse onto the same key.

       Two grades of key come out of this:

         strong - built from the whole name. A strong match is certain.
         weak   - built from the surname plus a first initial ("nramirez",
                  "ramirezn"). These catch a git author of "Nick Ramirez" against a
                  roster of "Ramirez, Nicholas", and a bare handle like
                  "jhollister" against "Hollister, Jordan" - which strong keys
                  miss completely, because a nickname shares no tokens with the
                  formal name. Weak keys can collide between two people, so the
                  roster drops any it hands to more than one person.

       Returns a hashtable of key -> 'strong'|'weak'. #>
    param([string] $Name, [string] $Email)

    $keys = @{}
    function Add-Key {
        param($Table, [string] $Key, [string] $Grade)
        if ($Key.Length -lt 4) { return }   # too short to mean anything
        if ($Grade -eq 'strong' -or -not $Table.ContainsKey($Key)) { $Table[$Key] = $Grade }
    }

    $sources = @()
    if ($Name)  { $sources += $Name }
    if ($Email) { $sources += ($Email -split '@')[0] }

    foreach ($src in $sources) {
        $clean  = ($src.ToLowerInvariant() -replace 'contractor', ' ') -replace '[^a-z]', ' '
        $tokens = @($clean -split '\s+' | Where-Object { $_.Length -gt 1 })
        if ($tokens.Count -eq 0) { continue }

        if ($tokens.Count -eq 1) {
            # A bare handle - "jhollister", "nramirez". Only a weak key is possible.
            Add-Key $keys $tokens[0] 'weak'
            continue
        }

        Add-Key $keys (($tokens | Sort-Object) -join '') 'strong'

        # "Last, First Middle" and "First Middle Last" put the surname at
        # opposite ends, so read the comma rather than guessing by position.
        if ($src -match ',') {
            $surname = $tokens[0]
            $given   = $tokens[1]
        }
        else {
            $given   = $tokens[0]
            $surname = $tokens[-1]
        }

        # Middle names and initials should not block a match.
        Add-Key $keys ((@($given, $surname) | Sort-Object) -join '') 'strong'

        # Nicknames and handles: Nick/Nicholas, Tom/Thomas, "jhollister".
        Add-Key $keys ($given.Substring(0, 1) + $surname) 'weak'
        Add-Key $keys ($surname + $given.Substring(0, 1)) 'weak'
    }
    return $keys
}

# --- event collection -------------------------------------------------------
# Events carry the raw identity. People are resolved after collection, once the
# roster is settled, so the auto-discover fallback costs nothing.

$Today    = (Get-Date).Date
$WindowTo = $Today.AddDays(1)
$From     = $Today.AddDays(-1 * ($DaysBack - 1))

$Events = New-Object System.Collections.ArrayList

function Add-Event {
    param(
        [string]   $Name,
        [string]   $Email,
        [datetime] $When,       # local time
        [string]   $Type,       # commit | merge | pr | comment | ticket
        [double]   $Weight,
        [string]   $Repo,
        [string]   $Detail,
        [string]   $Url
    )
    $day = $When.Date
    if ($day -lt $From -or $day -gt $Today) { return }

    [void]$Events.Add([pscustomobject]@{
        Name   = $Name
        Email  = $Email
        Day    = $day.ToString('yyyy-MM-dd')
        Type   = $Type
        Weight = [int][math]::Round($Weight, 0)
        Repo   = $Repo
        Detail = $Detail
        Url    = $Url
    })
}

function ConvertTo-Local {
    param($Value)
    if (-not $Value) { return $null }
    return ([datetime]$Value).ToLocalTime()
}

$fetchFrom = $From.AddDays(-1).ToString('yyyy-MM-dd')
$fetchTo   = $WindowTo.ToString('yyyy-MM-dd')

Write-Host ""
Write-Host "Snoop  ->  $Organization / $Project" -ForegroundColor Cyan
Write-Host "Window:  $($From.ToString('ddd dd MMM')) .. $($Today.ToString('ddd dd MMM'))  ($DaysBack days)" -ForegroundColor DarkGray
Write-Host ""

# --- 1 & 2: commits and merge commits --------------------------------------

$mergePattern = '^(Merge pull request|Merged PR|Merge branch|Merge remote-tracking|Merge commit)'

$SeenCommits = @{}

function Add-Commits {
    <# One pass over a repo, optionally pinned to a branch. Returns how many
       commits came back. Commits are deduped across branches by id. #>
    param([string] $Repo, [string] $Branch)

    $repoSeg = [uri]::EscapeDataString($Repo)
    $skip    = 0
    $page    = 1000          # the API caps $top here, so paging is mandatory
    $total   = 0

    $branchQuery = ''
    if ($Branch) {
        $branchQuery = "&searchCriteria.itemVersion.version=$([uri]::EscapeDataString($Branch))" +
                       "&searchCriteria.itemVersion.versionType=branch"
    }

    do {
        $uri = "$ApiBase/git/repositories/$repoSeg/commits" +
               "?searchCriteria.fromDate=$fetchFrom&searchCriteria.toDate=$fetchTo" +
               $branchQuery + "&`$top=$page&`$skip=$skip&api-version=7.1"
        try { $result = Invoke-Ado -Uri $uri }
        catch {
            if ($Branch) { Write-Warning "Branch '$Branch' in $Repo - $($_.Exception.Message)"; return $total }
            throw
        }
        $batch = @($result.value)

        foreach ($commit in $batch) {
            $seenKey = "$Repo|$($commit.commitId)"
            if ($commit.commitId -and $SeenCommits.ContainsKey($seenKey)) { continue }
            if ($commit.commitId) { $SeenCommits[$seenKey] = $true }

            # ADO hands back a commit if EITHER date lands in the range, so a
            # rebased or long-lived branch can arrive with an author date well
            # outside the window. Chart it on whichever date is actually in it.
            $authored  = ConvertTo-Local $commit.author.date
            $committed = ConvertTo-Local $commit.committer.date
            $when      = $authored
            if ((-not $when) -or $when.Date -lt $From -or $when.Date -gt $Today) {
                if ($committed -and $committed.Date -ge $From -and $committed.Date -le $Today) { $when = $committed }
            }
            if (-not $when) { continue }

            $lines = 0
            if ($commit.changeCounts) {
                $lines = [int]$commit.changeCounts.Add + [int]$commit.changeCounts.Edit + [int]$commit.changeCounts.Delete
            }
            $isMerge = $commit.comment -match $mergePattern
            $subject = ($commit.comment -split "`n")[0]
            if ($subject.Length -gt 90) { $subject = $subject.Substring(0, 90) + '...' }

            Add-Event -Name $commit.author.name -Email $commit.author.email -When $when `
                      -Type $(if ($isMerge) { 'merge' } else { 'commit' }) `
                      -Weight $lines -Repo $Repo -Detail "$subject  ($lines files changed)" -Url $commit.remoteUrl
        }

        $total += $batch.Count
        $skip  += $page
    } while ($batch.Count -eq $page)

    return $total
}

function Get-ActiveBranches {
    <# Branches whose tip moved inside the window. Anything older can only hold
       commits the default branch already has. #>
    param([string] $Repo)

    $repoSeg = [uri]::EscapeDataString($Repo)
    try { $stats = Invoke-Ado -Uri "$ApiBase/git/repositories/$repoSeg/stats/branches?api-version=7.1" }
    catch { Write-Warning "Branch list for $Repo - $($_.Exception.Message)"; return @() }

    $cutoff = $From.AddDays(-1)
    return @(
        $stats.value |
            Where-Object { $_.commit.committer.date -and (ConvertTo-Local $_.commit.committer.date) -ge $cutoff } |
            Sort-Object { ConvertTo-Local $_.commit.committer.date } -Descending |
            Select-Object -First $MaxBranches -ExpandProperty name
    )
}

foreach ($repo in $Repository) {
    Write-Host "Commits: $repo" -NoNewline
    $before = $Events.Count

    $total = Add-Commits -Repo $repo                 # default branch

    $branchCount = 0
    if ($AllBranches) {
        foreach ($branch in (Get-ActiveBranches -Repo $repo)) {
            $branchCount++
            Write-Progress -Activity "Branches in $repo" -Status $branch
            $total += Add-Commits -Repo $repo -Branch $branch
        }
        Write-Progress -Activity "Branches in $repo" -Completed
    }

    $suffix = if ($branchCount) { " (+$branchCount active branches)" } else { '' }
    Write-Host "  $total scanned$suffix, $($Events.Count - $before) in window" -ForegroundColor DarkGray
}

# --- 3 & 4: pull requests and PR comments -----------------------------------

$PullRequests = @{}

function Add-PullRequests {
    param([string] $Repo, [string] $Query)
    $repoSeg = [uri]::EscapeDataString($Repo)
    $skip    = 0
    $page    = 500
    do {
        $uri    = "$ApiBase/git/repositories/$repoSeg/pullrequests?$Query&`$top=$page&`$skip=$skip&api-version=7.1"
        $result = Invoke-Ado -Uri $uri
        $batch  = @($result.value)
        foreach ($pr in $batch) {
            $key = "$Repo|$($pr.pullRequestId)"
            if (-not $PullRequests.ContainsKey($key)) {
                $PullRequests[$key] = [pscustomobject]@{ Repo = $Repo; Pr = $pr }
            }
        }
        $skip += $page
    } while ($batch.Count -eq $page)
}

$prLookbackFrom = $From.AddDays(-1 * $PrLookbackDays).ToString('yyyy-MM-dd')

foreach ($repo in $Repository) {
    Write-Host "Pull requests: $repo" -NoNewline

    # Everything currently open, plus everything closed inside the lookback.
    # That is exactly the set that can hold a comment inside our window.
    Add-PullRequests -Repo $repo -Query 'searchCriteria.status=active'
    Add-PullRequests -Repo $repo -Query "searchCriteria.status=all&searchCriteria.queryTimeRangeType=closed&searchCriteria.minTime=$prLookbackFrom&searchCriteria.maxTime=$fetchTo"

    $count = @($PullRequests.Values | Where-Object { $_.Repo -eq $repo }).Count
    Write-Host "  $count" -ForegroundColor DarkGray
}

foreach ($entry in $PullRequests.Values) {
    $pr   = $entry.Pr
    $repo = $entry.Repo

    $created = ConvertTo-Local $pr.creationDate
    if ($created) {
        Add-Event -Name $pr.createdBy.displayName -Email $pr.createdBy.uniqueName -When $created `
                  -Type 'pr' -Weight 20 -Repo $repo `
                  -Detail "Opened PR $($pr.pullRequestId): $($pr.title)" -Url $pr.url
    }

    $closed = ConvertTo-Local $pr.closedDate
    if ($closed) {
        $closer = if ($pr.closedBy) { $pr.closedBy } else { $pr.createdBy }
        $verb   = if ($pr.status -eq 'abandoned') { 'Abandoned' } else { 'Completed' }
        Add-Event -Name $closer.displayName -Email $closer.uniqueName -When $closed `
                  -Type 'pr' -Weight 20 -Repo $repo `
                  -Detail "$verb PR $($pr.pullRequestId): $($pr.title)" -Url $pr.url
    }
}

if (-not $SkipPrComments) {
    $prList       = @($PullRequests.Values)
    $index        = 0
    $commentCount = 0
    $before       = $Events.Count

    foreach ($entry in $prList) {
        $index++
        Write-Progress -Activity 'PR comments' -Status "PR $($entry.Pr.pullRequestId) ($index of $($prList.Count))" `
                       -PercentComplete (100 * $index / [math]::Max($prList.Count, 1))

        $repoSeg = [uri]::EscapeDataString($entry.Repo)
        $uri = "$ApiBase/git/repositories/$repoSeg/pullRequests/$($entry.Pr.pullRequestId)/threads?api-version=7.1"
        try { $threads = Invoke-Ado -Uri $uri }
        catch { Write-Warning "Threads for PR $($entry.Pr.pullRequestId): $($_.Exception.Message)"; continue }

        foreach ($thread in @($threads.value)) {
            foreach ($comment in @($thread.comments)) {
                if ($comment.commentType -eq 'system' -or $comment.isDeleted) { continue }
                $when = ConvertTo-Local $comment.publishedDate
                if (-not $when) { continue }

                $text = ($comment.content -replace '\s+', ' ')
                if ($text.Length -gt 90) { $text = $text.Substring(0, 90) + '...' }

                Add-Event -Name $comment.author.displayName -Email $comment.author.uniqueName -When $when `
                          -Type 'comment' -Weight $comment.content.Length -Repo $entry.Repo `
                          -Detail "PR $($entry.Pr.pullRequestId): $text" -Url $entry.Pr.url
                $commentCount++
            }
        }
    }
    Write-Progress -Activity 'PR comments' -Completed
    Write-Host "PR comments:  $commentCount scanned, $($Events.Count - $before) in window" -ForegroundColor DarkGray
}

# --- 5: work items ----------------------------------------------------------

if (-not $SkipWorkItems) {
    $noiseFields = @(
        'System.Rev', 'System.ChangedDate', 'System.ChangedBy', 'System.AuthorizedAs',
        'System.AuthorizedDate', 'System.RevisedDate', 'System.Watermark', 'System.PersonId',
        'System.CommentCount'
    )

    $ticketEvents = 0
    $before       = $Events.Count

    foreach ($wiProject in $WorkItemProjects) {

    $wiBase  = "https://dev.azure.com/$WorkItemOrg/$([uri]::EscapeDataString($wiProject))/_apis"
    $wiLabel = if ($WorkItemOrg -eq $Organization -and $wiProject -eq $Project) { $wiProject } else { "$WorkItemOrg/$wiProject" }

    $where = "[System.TeamProject] = '$($wiProject -replace "'", "''")' " +
             "AND [System.ChangedDate] >= '$($From.ToString('yyyy-MM-dd'))'"

    # A shared board can hold thousands of items that have nothing to do with
    # this team, so allow narrowing to the area paths they actually work under.
    if ($WorkItemAreaPaths.Count -gt 0) {
        $clauses = @($WorkItemAreaPaths | ForEach-Object { "[System.AreaPath] UNDER '$($_ -replace "'", "''")'" })
        $where  += " AND (" + ($clauses -join ' OR ') + ")"
    }

    $wiql = @{
        query = "SELECT [System.Id] FROM WorkItems WHERE $where ORDER BY [System.ChangedDate] DESC"
    } | ConvertTo-Json

    $ids = @()
    try {
        $wiqlResult = Invoke-Ado -Uri "$wiBase/wit/wiql?api-version=7.1" -Method Post -Body $wiql
        $ids = @($wiqlResult.workItems | ForEach-Object { $_.id })
    }
    catch { Write-Warning "Work item query against $wiLabel failed: $($_.Exception.Message)" }

    Write-Host "Work items: $wiLabel  $($ids.Count)" -NoNewline
    $index = 0

    foreach ($workItemId in $ids) {
        $index++
        Write-Progress -Activity "Work item history ($wiLabel)" -Status "#$workItemId ($index of $($ids.Count))" `
                       -PercentComplete (100 * $index / [math]::Max($ids.Count, 1))

        try { $updates = Invoke-Ado -Uri "$wiBase/wit/workitems/$workItemId/updates?api-version=7.1" }
        catch { continue }

        foreach ($update in @($updates.value)) {
            if (-not $update.revisedBy) { continue }

            $stamp = $null
            if ($update.fields -and $update.fields.'System.ChangedDate') { $stamp = $update.fields.'System.ChangedDate'.newValue }
            if (-not $stamp) { $stamp = $update.revisedDate }
            $when = ConvertTo-Local $stamp
            # ADO writes a year-9999 sentinel on some revisions.
            if (-not $when -or $when.Year -gt 9000) { continue }

            $changed = @()
            if ($update.fields) {
                $changed = @($update.fields.PSObject.Properties.Name | Where-Object { $noiseFields -notcontains $_ })
            }
            $hasRelations = $null -ne $update.relations
            if ($changed.Count -eq 0 -and -not $hasRelations) { continue }

            $summary = if ($changed.Count -gt 0) {
                (($changed | ForEach-Object { ($_ -split '\.')[-1] }) -join ', ')
            } else { 'links' }
            if ($summary.Length -gt 80) { $summary = $summary.Substring(0, 80) + '...' }

            Add-Event -Name $update.revisedBy.displayName -Email $update.revisedBy.uniqueName -When $when `
                      -Type 'ticket' -Weight ([math]::Max($changed.Count, 1)) -Repo '(work items)' `
                      -Detail "#$workItemId  $summary" `
                      -Url "https://dev.azure.com/$WorkItemOrg/$([uri]::EscapeDataString($wiProject))/_workitems/edit/$workItemId"
            $ticketEvents++
        }
    }
    Write-Progress -Activity "Work item history ($wiLabel)" -Completed
    Write-Host "  ->  $ticketEvents revisions scanned so far" -ForegroundColor DarkGray

    }   # next work item project

    Write-Host "Ticket activity: $($Events.Count - $before) in window" -ForegroundColor DarkGray
}

# ---------------------------------------------------------------------------
# Resolve identities -> roster
# ---------------------------------------------------------------------------

# Every distinct identity that appeared, with a volume count.
$Identities = @{}
foreach ($evt in $Events) {
    $key = "$($evt.Name)|$($evt.Email)"
    if (-not $Identities.ContainsKey($key)) {
        $Identities[$key] = [pscustomobject]@{
            Name  = $evt.Name
            Email = $evt.Email
            Keys  = (Get-NameKeys -Name $evt.Name -Email $evt.Email)
            Count = 0
            Types = @{}
        }
    }
    $Identities[$key].Count++
    $Identities[$key].Types[$evt.Type] = [int]$Identities[$key].Types[$evt.Type] + 1
}

function Build-Roster {
    param([string[]] $Names)
    $roster = @()
    $index  = 0
    foreach ($name in $Names) {
        $roster += [pscustomobject]@{
            Id      = $index
            Name    = $name
            Keys    = (Get-NameKeys -Name $name)
            Matched = $false
        }
        $index++
    }

    # A weak key that two people share ("jsmith" for John and Jane Smith) would
    # silently send one person's commits to the other. Drop those outright and
    # let the alias list settle it instead.
    $weakOwners = @{}
    foreach ($person in $roster) {
        foreach ($key in @($person.Keys.Keys)) {
            if ($person.Keys[$key] -ne 'weak') { continue }
            if (-not $weakOwners.ContainsKey($key)) { $weakOwners[$key] = @() }
            $weakOwners[$key] += $person.Id
        }
    }
    foreach ($key in $weakOwners.Keys) {
        if (@($weakOwners[$key]).Count -lt 2) { continue }
        foreach ($person in $roster) { $person.Keys.Remove($key) }
    }

    return $roster
}

function Find-RosterMatch {
    <# Returns the roster id matching this identity, or -1. Strong keys are tried
       across the whole roster before any weak key is considered, so a nickname
       guess can never outrank a real name match. #>
    param($Identity, $Roster, [string] $Grade)

    foreach ($person in $Roster) {
        foreach ($key in @($Identity.Keys.Keys)) {
            if ($Identity.Keys[$key] -ne $Grade) { continue }
            if ($person.Keys.Contains($key) -and $person.Keys[$key] -eq $Grade) { return $person.Id }
        }
    }
    return -1
}

function Resolve-Identities {
    param($Roster)
    # identity key string -> roster id
    $map   = @{}
    $weak  = @()
    $pending = @()

    foreach ($identity in $Identities.Values) {
        $identityKey = "$($identity.Name)|$($identity.Email)"
        $resolved    = -1

        foreach ($candidate in @($identity.Name, $identity.Email)) {
            if ($candidate -and $Aliases.ContainsKey($candidate)) {
                $target = $Roster | Where-Object { $_.Name -eq $Aliases[$candidate] } | Select-Object -First 1
                if ($target) { $resolved = $target.Id; break }
            }
        }

        if ($resolved -lt 0) { $resolved = Find-RosterMatch -Identity $identity -Roster $Roster -Grade 'strong' }

        if ($resolved -ge 0) {
            ($Roster | Where-Object { $_.Id -eq $resolved }).Matched = $true
            $map[$identityKey] = $resolved
        }
        else {
            $pending += $identity    # give every strong match its chance first
        }
    }

    foreach ($identity in $pending) {
        $identityKey = "$($identity.Name)|$($identity.Email)"
        $resolved    = Find-RosterMatch -Identity $identity -Roster $Roster -Grade 'weak'
        if ($resolved -ge 0) {
            $person = $Roster | Where-Object { $_.Id -eq $resolved }
            $person.Matched = $true
            $weak += [pscustomobject]@{ Name = $identity.Name; Email = $identity.Email; Person = $person.Name; Count = $identity.Count }
        }
        $map[$identityKey] = $resolved
    }

    $script:WeakMatches = $weak
    return $map
}

$Roster = @(Build-Roster -Names $People)
$IdentityMap = Resolve-Identities -Roster $Roster

$matchedCount = @($Roster | Where-Object { $_.Matched }).Count

# Three ways we can end up charting discovered people instead of a roster:
#   discovered - no roster was configured, so show whoever is active here
#   fallback   - a roster was configured but matched nobody, which usually
#                means Snoop is pointed at the wrong org/project
#   roster     - normal case
$RosterMode = if ($DiscoverMode) { 'discovered' } elseif ($matchedCount -eq 0) { 'fallback' } else { 'roster' }

if ($RosterMode -ne 'roster') {

    # One human often shows up as several identities (git name "jhollister",
    # display name "Jordan Hollister", an email). Cluster them on shared name
    # keys first, so the discovered roster gets one row per person.
    $clusters = @()
    foreach ($identity in ($Identities.Values | Sort-Object Count -Descending)) {
        $target = $null
        foreach ($cluster in $clusters) {
            foreach ($key in @($identity.Keys.Keys)) {
                if ($cluster.Keys.Contains($key)) { $target = $cluster; break }
            }
            if ($target) { break }
        }
        if (-not $target) {
            $target = [pscustomobject]@{
                Keys  = [System.Collections.Generic.HashSet[string]]::new()
                Names = @()
                Count = 0
            }
            $clusters += $target
        }
        foreach ($key in @($identity.Keys.Keys)) { [void]$target.Keys.Add($key) }
        $target.Names += $(if ($identity.Name) { $identity.Name } else { $identity.Email })
        $target.Count += $identity.Count
    }

    # Label each cluster with a real display name where one exists, rather than
    # whichever git handle happened to sort first.
    $discoveredNames = @(
        $clusters | Sort-Object Count -Descending | Select-Object -First $MaxDiscovered | ForEach-Object {
            $pretty = @($_.Names | Where-Object { $_ -match '\s' })
            if ($pretty.Count -gt 0) { $pretty[0] } else { $_.Names[0] }
        }
    )

    $Roster      = @(Build-Roster -Names $discoveredNames)
    $IdentityMap = Resolve-Identities -Roster $Roster
}

function Get-NameTokens {
    param([string] $Text)
    if (-not $Text) { return @() }
    $clean = ($Text.ToLowerInvariant() -replace 'contractor', ' ') -replace '[^a-z]', ' '
    return @($clean -split '\s+' | Where-Object { $_.Length -gt 2 })
}

$Unmatched = @()
foreach ($identity in $Identities.Values) {
    $identityKey = "$($identity.Name)|$($identity.Email)"
    if ($IdentityMap[$identityKey] -ge 0) { continue }

    # If this identity shares a surname-ish token with somebody on the roster,
    # say so - that is nearly always the alias the user needs to add.
    $tokens  = @(Get-NameTokens "$($identity.Name) $(($identity.Email -split '@')[0])")
    $suggest = ''
    foreach ($person in $Roster) {
        $shared = @(Get-NameTokens $person.Name | Where-Object { $tokens -contains $_ })
        if ($shared.Count -gt 0) { $suggest = $person.Name; break }
    }

    $Unmatched += [pscustomobject]@{
        Name    = $identity.Name
        Email   = $identity.Email
        Count   = $identity.Count
        Types   = (($identity.Types.GetEnumerator() | Sort-Object Value -Descending |
                    ForEach-Object { "$($_.Value) $($_.Key)" }) -join ', ')
        Suggest = $suggest
    }
}

# ---------------------------------------------------------------------------
# Day columns
# ---------------------------------------------------------------------------

$activeDays = @{}
foreach ($evt in $Events) { $activeDays[$evt.Day] = $true }

$Days = @()
for ($cursor = $From; $cursor -le $Today; $cursor = $cursor.AddDays(1)) {
    $key       = $cursor.ToString('yyyy-MM-dd')
    $isWeekend = $cursor.DayOfWeek -eq 'Saturday' -or $cursor.DayOfWeek -eq 'Sunday'

    # Weekends are hidden unless something actually happened on them.
    if ($isWeekend -and -not $IncludeWeekends -and -not $activeDays.ContainsKey($key)) { continue }

    $Days += [ordered]@{
        key     = $key
        label   = $(if ($cursor -eq $Today) { 'TODAY' } else { $cursor.ToString('ddd dd').ToUpper() })
        weekend = $isWeekend
    }
}

# ---------------------------------------------------------------------------
# Payload + render
# ---------------------------------------------------------------------------

$compactEvents = foreach ($evt in $Events) {
    [ordered]@{
        p = $IdentityMap["$($evt.Name)|$($evt.Email)"]
        d = $evt.Day
        t = $evt.Type
        w = $evt.Weight
        r = $evt.Repo
        x = $evt.Detail
        u = $evt.Url
    }
}

$payload = [ordered]@{
    meta = [ordered]@{
        org       = $Organization
        project   = $Project
        repos     = @($Repository)
        from      = $From.ToString('yyyy-MM-dd')
        to        = $Today.ToString('yyyy-MM-dd')
        days      = $DaysBack
        generated = (Get-Date).ToString('yyyy-MM-dd HH:mm')
        mode      = $RosterMode
        config    = (Split-Path -Leaf $ConfigFile)
        tickets   = $(if ($SkipWorkItems) { '' } else { ($WorkItemProjects | ForEach-Object { "$WorkItemOrg/$_" }) -join ', ' })
        branches  = [bool]$AllBranches
        command   = ".\Snoop.ps1 -Organization $Organization -Project $Project -Repository $($Repository -join ',') -DaysBack $DaysBack"
    }
    days      = @($Days)
    people    = @($Roster | ForEach-Object { [ordered]@{ id = $_.Id; name = $_.Name; matched = $_.Matched } })
    events    = @($compactEvents)
    unmatched = @($Unmatched | Sort-Object Count -Descending | Select-Object -First 40 |
                  ForEach-Object { [ordered]@{ name = $_.Name; email = $_.Email; count = $_.Count
                                               types = $_.Types; suggest = $_.Suggest } })
    weak      = @($WeakMatches | Sort-Object Count -Descending |
                  ForEach-Object { [ordered]@{ name = $_.Name; email = $_.Email; person = $_.Person; count = $_.Count } })
}

$json = $payload | ConvertTo-Json -Depth 8 -Compress
# Keep a stray "</script>" inside a commit message from ending the script block.
# "<" is a valid escape inside a JSON string, so the data survives intact.
$json = $json.Replace('<', ('\' + 'u003c'))

$templatePath = Join-Path $PSScriptRoot 'template.html'
if (-not (Test-Path -LiteralPath $templatePath)) { throw "Missing template.html next to Snoop.ps1 (looked in $PSScriptRoot)." }

$html = (Get-Content -Raw -LiteralPath $templatePath).Replace('__SNOOP_DATA__', $json)

if (-not $OutFile) { $OutFile = Join-Path $PSScriptRoot 'snoop.html' }
Set-Content -LiteralPath $OutFile -Value $html -Encoding UTF8

Write-Host ""
Write-Host "Events:  $($Events.Count)  across $($Roster.Count) people, $($Days.Count) day columns" -ForegroundColor Green
Write-Host "Written: $OutFile" -ForegroundColor Green
switch ($RosterMode) {
    'discovered' {
        Write-Host "People:  no roster configured, so Snoop charted the $($Roster.Count) most active it found." -ForegroundColor DarkGray
        Write-Host "         Add names to 'people' in $(Split-Path -Leaf $ConfigFile) to pin the list." -ForegroundColor DarkGray
    }
    'fallback' {
        Write-Host "Note:    none of the configured names matched this project, so Snoop charted the people it found instead." -ForegroundColor Yellow
    }
}

# The quiet failure this guards against: somebody's PRs and tickets land on their
# row because ADO knows their display name, while their commits sit under a git
# author name nobody recognised. The row looks real, just short of commits.
$orphanCommits = @($Unmatched | Where-Object { $_.Types -match 'commit|merge' } | Sort-Object Count -Descending)
if ($RosterMode -eq 'roster' -and $orphanCommits.Count -gt 0) {
    Write-Host ""
    Write-Host "Unattributed commit authors - these are NOT on anyone's row:" -ForegroundColor Yellow
    foreach ($orphan in ($orphanCommits | Select-Object -First 10)) {
        $hint = if ($orphan.Suggest) { "   <- probably '$($orphan.Suggest)'" } else { '' }
        Write-Host ("  {0,-28} {1,-34} {2}{3}" -f $orphan.Name, $orphan.Email, $orphan.Types, $hint) -ForegroundColor DarkGray
    }
    Write-Host "  Add them under 'aliases' in $(Split-Path -Leaf $ConfigFile), e.g." -ForegroundColor DarkGray
    $example = $orphanCommits[0]
    $target  = if ($example.Suggest) { $example.Suggest } else { '<roster name>' }
    Write-Host "    `"aliases`": { `"$($example.Name)`": `"$target`" }" -ForegroundColor DarkGray
}

if ($WeakMatches.Count -gt 0) {
    Write-Host ""
    Write-Host "Matched on a nickname/handle guess (check these):" -ForegroundColor DarkGray
    foreach ($guess in $WeakMatches) {
        Write-Host ("  {0,-28} -> {1}  ({2} events)" -f $guess.Name, $guess.Person, $guess.Count) -ForegroundColor DarkGray
    }
}
Write-Host ""

if (-not $NoOpen) { Start-Process $OutFile }
