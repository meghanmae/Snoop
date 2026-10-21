<#
    One-off: count pull requests and work items per person over the last N months.

    PRs:        created by the person in the configured repos, created inside the
                window, excluding drafts and abandoned PRs.
    Work items: assigned to the person in the configured work item projects,
                changed inside the window, excluding the Removed state.
                Broken out by work item type.
    Stories:    User Stories / PBIs assigned to the person, in a Closed/Done state,
                closed inside the window, with their story points (StoryPoints, or
                Effort on Scrum).

    Auth matches Snoop.ps1: $env:ADO_PAT if set, otherwise 'az login'.
#>
param(
    [string] $ConfigPath = (Join-Path $PSScriptRoot 'snoop.config.json'),
    [int]    $MonthsBack = 6
)

$ErrorActionPreference = 'Stop'

# Keep the window open on failure so the error can be read.
trap {
    Write-Host ""
    Write-Host "ERROR: $($_.Exception.Message)" -ForegroundColor Red
    Write-Host $_.InvocationInfo.PositionMessage -ForegroundColor DarkGray
    Read-Host 'Press Enter to exit'
    exit 1
}

$config  = Get-Content $ConfigPath -Raw | ConvertFrom-Json
$org     = $config.organization
$project = $config.project
$repos   = @($config.repositories)
# The repos also come from the active teams.
if ($config.teams) {
    foreach ($team in $config.teams.PSObject.Properties.Value) {
        if ($team -is [array] -or $team -is [string]) { $repos += @($team); continue }
        if ($team.PSObject.Properties['enabled'] -and $team.enabled -eq $false) { continue }
        $repos += @($team.repositories)
    }
}
$seenRepos = @{}
$repos   = @($repos | Where-Object { $_ -and -not $seenRepos.ContainsKey("$_".ToLower()) } | ForEach-Object { $seenRepos["$_".ToLower()] = $true; "$_" })
$people  = @($config.people)
$wiOrg   = if ($config.workItems.organization) { $config.workItems.organization } else { $org }
$wiProjects  = @($config.workItems.projects)
$wiAreaPaths = @($config.workItems.areaPaths)

$since    = (Get-Date).Date.AddMonths(-1 * $MonthsBack)
$sinceIso = $since.ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')

# --- auth ---------------------------------------------------------------------

if ($env:ADO_PAT) {
    $raw = [Text.Encoding]::ASCII.GetBytes(":$($env:ADO_PAT)")
    $Headers = @{ Authorization = 'Basic ' + [Convert]::ToBase64String($raw); 'Content-Type' = 'application/json' }
}
else {
    $token = az account get-access-token --resource '499b84ac-1321-427f-aa17-267ca6975798' --query accessToken -o tsv 2>$null
    if ([string]::IsNullOrWhiteSpace($token)) {
        throw "Could not get an Azure DevOps token. Run 'az login' or set `$env:ADO_PAT."
    }
    $Headers = @{ Authorization = "Bearer $token"; 'Content-Type' = 'application/json' }
}

function Invoke-Ado {
    param([string] $Uri, [string] $Method = 'GET', $Body)
    if ($Body) { return Invoke-RestMethod -Uri $Uri -Headers $Headers -Method $Method -Body $Body }
    return Invoke-RestMethod -Uri $Uri -Headers $Headers -Method $Method
}

# --- tallies ------------------------------------------------------------------

$tally = [ordered]@{}
foreach ($person in $people) {
    $tally[$person] = @{
        PRs = 0; WorkItems = 0; Types = @{}
        StoriesClosed = 0; PointsClosed = 0.0; UnpointedClosed = 0
    }
}

# Work item types that carry story points. Agile = User Story, Scrum = Product Backlog Item.
$storyTypes   = @('User Story', 'Product Backlog Item', 'Story')
$closedStates = @('Closed', 'Done')
$allTypes     = @{}
$storyStates  = @{}

function Find-Person([string] $displayName) {
    foreach ($person in $people) {
        if ($person -ieq $displayName) { return $person }
    }
    return $null
}

# --- pull requests ------------------------------------------------------------

$apiBase = "https://dev.azure.com/$org/$([uri]::EscapeDataString($project))/_apis"

foreach ($repo in $repos) {
    Write-Host "PRs: $repo" -NoNewline
    $repoSeg = [uri]::EscapeDataString($repo)
    $skip  = 0
    $page  = 500
    $found = 0
    do {
        $uri = "$apiBase/git/repositories/$repoSeg/pullrequests" +
               "?searchCriteria.status=all&searchCriteria.queryTimeRangeType=created" +
               "&searchCriteria.minTime=$sinceIso&`$top=$page&`$skip=$skip&api-version=7.1"
        $batch = @((Invoke-Ado -Uri $uri).value)

        foreach ($pr in $batch) {
            if ($pr.isDraft -or $pr.status -eq 'abandoned') { continue }
            $person = Find-Person $pr.createdBy.displayName
            if ($person) { $tally[$person].PRs++; $found++ }
        }
        $skip += $page
    } while ($batch.Count -eq $page)
    Write-Host "  $found" -ForegroundColor DarkGray
}

# --- work items ---------------------------------------------------------------

foreach ($wiProject in $wiProjects) {
    $wiBase = "https://dev.azure.com/$wiOrg/$([uri]::EscapeDataString($wiProject))/_apis"

    foreach ($person in $people) {
        $where = "[System.TeamProject] = '$($wiProject -replace "'", "''")'" +
                 " AND [System.AssignedTo] = '$($person -replace "'", "''")'" +
                 " AND [System.State] <> 'Removed'" +
                 " AND [System.ChangedDate] >= '$($since.ToString('yyyy-MM-dd'))'"
        if ($wiAreaPaths.Count -gt 0) {
            $clauses = @($wiAreaPaths | ForEach-Object { "[System.AreaPath] UNDER '$($_ -replace "'", "''")'" })
            $where  += " AND (" + ($clauses -join ' OR ') + ")"
        }

        $body = @{ query = "SELECT [System.Id] FROM WorkItems WHERE $where" } | ConvertTo-Json
        try {
            $result = Invoke-Ado -Uri "$wiBase/wit/wiql?api-version=7.1" -Method Post -Body $body
            $ids    = @($result.workItems | ForEach-Object { [int]$_.id })
        }
        catch {
            Write-Warning "Work item query for '$person' in $wiProject failed: $($_.Exception.Message)"
            $ids = @()
        }

        # WIQL only returns ids; pull the fields in batches of 200.
        for ($i = 0; $i -lt $ids.Count; $i += 200) {
            $chunk = $ids[$i..([math]::Min($i + 199, $ids.Count - 1))]
            $batchBody = @{ ids = $chunk } | ConvertTo-Json
            $items = @((Invoke-Ado -Uri "$wiBase/wit/workitemsbatch?api-version=7.1" -Method Post -Body $batchBody).value)

            foreach ($item in $items) {
                $f    = $item.fields
                $type = $f.'System.WorkItemType'
                $t    = $tally[$person]

                $t.WorkItems++
                $t.Types[$type] = [int]$t.Types[$type] + 1
                $allTypes[$type] = [int]$allTypes[$type] + 1

                if ($storyTypes -notcontains $type) { continue }

                $state = $f.'System.State'
                $storyStates[$state] = [int]$storyStates[$state] + 1
                if ($closedStates -notcontains $state) { continue }

                # The query already limits to items changed in the window; this also
                # drops stories closed before the window and merely touched since.
                $closed = $f.'Microsoft.VSTS.Common.ClosedDate'
                if ($closed -and [datetime]$closed -lt $since) { continue }

                # Agile/CMMI use StoryPoints, Scrum uses Effort.
                $points = $f.'Microsoft.VSTS.Scheduling.StoryPoints'
                if ($null -eq $points) { $points = $f.'Microsoft.VSTS.Scheduling.Effort' }

                $t.StoriesClosed++
                if ($null -eq $points) { $t.UnpointedClosed++ }
                else { $t.PointsClosed += [double]$points }
            }
        }

        Write-Host "Work items: $wiProject / $person  $($ids.Count)" -ForegroundColor DarkGray
    }
}

# --- report -------------------------------------------------------------------

$rows = foreach ($person in $tally.Keys) {
    [pscustomobject]@{
        Person    = $person
        PRs       = [int]$tally[$person].PRs
        WorkItems = [int]$tally[$person].WorkItems
        Total     = [int]$tally[$person].PRs + [int]$tally[$person].WorkItems
    }
}
$rows = @($rows | Sort-Object -Property Total -Descending)

$rows += [pscustomobject]@{
    Person    = 'TOTAL'
    PRs       = ($rows | Measure-Object PRs -Sum).Sum
    WorkItems = ($rows | Measure-Object WorkItems -Sum).Sum
    Total     = ($rows | Measure-Object Total -Sum).Sum
}

Write-Host ""
Write-Host "Since $($since.ToString('yyyy-MM-dd')) ($MonthsBack months), sorted by total" -ForegroundColor Cyan
$rows | Format-Table -AutoSize | Out-Host

# Work items by type, most common type first.
$typeOrder = @($allTypes.Keys | Sort-Object { $allTypes[$_] } -Descending)
$typeRows = foreach ($person in $tally.Keys) {
    $row = [ordered]@{ Person = $person }
    foreach ($type in $typeOrder) { $row[$type] = [int]$tally[$person].Types[$type] }
    $row['Total'] = [int]$tally[$person].WorkItems
    [pscustomobject]$row
}
$typeRows = @($typeRows | Sort-Object -Property Total -Descending)

Write-Host "Work items by type" -ForegroundColor Cyan
$typeRows | Format-Table -AutoSize | Out-Host

# Stories closed in the window vs the points on them.
$storyRows = foreach ($person in $tally.Keys) {
    $t = $tally[$person]
    $pointed = $t.StoriesClosed - $t.UnpointedClosed
    [pscustomobject]@{
        Person        = $person
        StoriesClosed = [int]$t.StoriesClosed
        Points        = [double]$t.PointsClosed
        AvgPoints     = if ($pointed -gt 0) { [math]::Round($t.PointsClosed / $pointed, 1) } else { 0 }
        Unpointed     = [int]$t.UnpointedClosed
    }
}
$storyRows = @($storyRows | Sort-Object -Property Points -Descending)

Write-Host "Stories closed since $($since.ToString('yyyy-MM-dd')), sorted by points" -ForegroundColor Cyan
$storyRows | Format-Table -AutoSize | Out-Host

$stateSummary = ($storyStates.GetEnumerator() | Sort-Object Value -Descending | ForEach-Object { "$($_.Key): $($_.Value)" }) -join ', '
Write-Host "Story states seen (counted as closed: $($closedStates -join ', ')): $stateSummary" -ForegroundColor DarkGray
Write-Host ""

Read-Host 'Press Enter to exit'
