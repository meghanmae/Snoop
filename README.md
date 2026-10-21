# Snoop

A vibe coded super minimal/hacky stand-in for the activity dashboard in Appfire Flow, which is being retired.

Point it at an Azure DevOps repo, and it charts who did what, day by day: commits,
merges, PRs, PR comments and ticket activity. No database, no login, no server.
Run it, look at it, close it.

| Row | Colour | Source | Pill size means |
|---|---|---|---|
| Code commit | blue | non-merge commits | files changed |
| Merge commit | yellow | commits whose message starts with `Merge …` | files changed |
| PR activity | purple | PRs opened / completed / abandoned | fixed |
| PR comment | red | human comments on PR threads | comment length |
| Ticket activity | green | work item revisions | fields changed (dots) |

---

## Quick start: snoop.exe

```powershell
.\snoop.exe
```

It reads `snoop.config.json` (from the current folder, else next to the exe -
a starter one is written there on first run), opens the dashboard in your
browser and fetches. Press **Refresh** in the sidebar to fetch again; it
re-reads the config too, so edit it and refresh rather than restarting. Close
the console window to stop it.

Requests run side by side, so a refresh takes seconds rather than minutes.
Two extra settings control the exe:

| Setting | |
|---|---|
| `maxConcurrentRequests` | Requests in flight at once (default 12). Lower it if ADO starts throttling. |
| `port` | Local port for the dashboard (default 7414; any free port if taken) |

```powershell
.\snoop.exe --config .\client.config.json   # another config
.\snoop.exe --port 8080 --no-open           # different port, no browser
```

Auth is the same as the script: `ADO_PAT` if set, otherwise `az login`.

### Building it

One-time setup (no admin rights needed):

```powershell
winget install Rustlang.Rustup
rustup default stable-x86_64-pc-windows-gnu
winget install BrechtSanders.WinLibs.POSIX.UCRT   # provides dlltool; add its mingw64\bin to PATH
```

Then `cargo build --release` gives `target\release\snoop.exe`, a single file
with the dashboard built in. Copy it wherever you like.

## Quick start: the PowerShell script

The original script still works and needs nothing installed:

```powershell
cd C:\tools\Snoop
.\Snoop.ps1
```

That's it. The first run creates `snoop.config.json` next to the script, fetches the
last 14 days, writes `snoop.html` and opens it in your browser.

A run takes about 30 seconds on a busy repo. Re-run it whenever you want fresh data —
there's nothing running in the background.

**If PowerShell blocks the script** (`running scripts is disabled on this system`):

```powershell
powershell -ExecutionPolicy Bypass -File .\Snoop.ps1
```

### Requirements

- Windows PowerShell 5.1 (already on your machine) — nothing to install
- Azure CLI, already logged in: `az login`
- A browser

---

## Configuring it

Everything lives in **`snoop.config.json`**, created on first run:

```json
{
  "organization": "contoso",
  "project": "Apollo",
  "repositories": [ "Apollo" ],
  "daysBack": 14,
  "people": [],
  "aliases": {},
  "workItems": {
    "organization": "",
    "projects": [],
    "areaPaths": []
  },
  "maxDiscoveredPeople": 20,
  "prLookbackDays": 90,
  "includeAllBranches": true,
  "maxBranches": 50,
  "includeWeekends": false
}
```

| Setting | What it does |
|---|---|
| `organization` | ADO org name, the bit after `dev.azure.com/` |
| `project` | Project name inside that org |
| `repositories` | One or more repos. All of them get charted together. |
| `daysBack` | Size of the window, in days |
| `people` | Who to chart. **Leave it empty to auto-pick the most active people.** |
| `aliases` | Fixes for people whose git handle doesn't resemble their name |
| `workItems` | Where the tickets live, if it isn't here. See below. |
| `maxDiscoveredPeople` | Cap when `people` is empty (default 20) |
| `prLookbackDays` | How far back to scan PRs for recent comments |
| `includeAllBranches` | Scan feature branches too, not just the default branch |
| `maxBranches` | Cap on branches scanned per repo (default 50) |
| `includeWeekends` | Keep empty Sat/Sun columns instead of hiding them |

### When the tickets live somewhere else

Code and boards often aren't in the same place. Leave `workItems` blank and Snoop
looks for tickets in the same project as the repo. Fill it in when they're apart:

```json
"workItems": {
  "organization": "contoso",
  "projects": [ "Delivery", "Platform" ],
  "areaPaths": [ "Delivery\\TeamApollo" ]
}
```

| Field | |
|---|---|
| `organization` | Blank means "same org as the code". Set it when the boards are in another org. |
| `projects` | Blank means "same project as the code". List as many as you like — all get charted together. |
| `areaPaths` | Optional. Only count tickets under these area paths. |

`areaPaths` matters when you point at a big shared project: without it Snoop pulls
the revision history of *every* item that changed in the window, which is slow and
mostly noise. Backslashes need doubling in JSON (`"Delivery\\Business"`).

For one run, without touching the config:

```powershell
.\Snoop.ps1 -WorkItemOrganization contoso -WorkItemProject Delivery,Platform
```

If the boards are in a different **org** than the code, your `az login` usually
still covers both, as long as they're in the same tenant. If they aren't, use a PAT
(see [Auth](#auth)) — but note a PAT is issued per org, so split the run in two, or
use a token that org accepts.

### Teams, velocity and forecasts

The **Teams** tab groups repos into teams and answers "when will this be done?".
List each team's repos under `teams`:

```json
"teams": {
  "Billing": [
    "Contoso.Web.Billing",
    "Contoso.Api.Billing",
    "Contoso.Mobile.Billing"
  ],
  "Scheduling": {
    "repositories": [ "Contoso.Web.Scheduling", "Contoso.Api.Scheduling" ],
    "titlePrefixes": [ "SCH" ]
  },
  "Docs": {
    "repositories": [ "Contoso.Web.Docs", "Contoso.Api.Docs" ],
    "areaPaths": [ "Apollo\\Docs" ],
    "members": [ "Fletcher, Quinn", "Grant, Sam" ]
  }
}
```

A bare list of repos is enough. The longer form fixes the two guesses Snoop makes:

| Field | Without it |
|---|---|
| `members` | anyone on the roster with a PR in the team's repos |
| `areaPaths` / `titlePrefixes` | the team's backlog is the open stories assigned to its members, so unassigned stories are missed |

Use `titlePrefixes` when several teams share one backlog and name their stories
`CG - ...`, `CG: ...` or `[CG] ...`. Matching ignores case and needs the whole tag,
so `CG` doesn't catch `CGI ...`. A task belongs to its parent story's team. A team
can have both: a story under its area paths *or* with its prefix counts.

No `teams` at all gives one team made of everyone on the roster.

The Teams tab looks back much further than `daysBack`, set under `stats`:

| Setting | |
|---|---|
| `monthsBack` | How far back to read PRs and closed stories (default 6) |
| `sprintLengthDays` | Sprint length (default 14) |
| `sprintAnchorDate` | The first day of any one sprint, e.g. `"2026-01-05"`, so the periods line up with your real sprints and the sprint in progress stays out of the average. Without it, sprints are rolling periods ending today. |
| `velocitySprints` | How many recent sprints the forecast averages (default 6; also switchable on the page) |
| `storyTypes` | Work item types that carry points (default User Story / PBI / Story) |
| `hoursPerDevDay` | Converts task Remaining Work hours into dev-days (default 6) |

For each team it shows:

- **Velocity**: points closed per sprint, averaged over recent sprints, with a slow
  and fast figure from the slower and faster halves of those sprints. Teams that
  don't point their stories get a story count instead.
- **Open work**: points on every story not Done or Removed (state categories come
  from ADO, so Closed, Done, Cut etc. all work), split into not started, in
  progress and resolved. Unpointed stories count at the team's average story size.
- **Likely finish**: open work ÷ velocity, as working days and a date, with the
  slow–fast range. Also shown as **dev-days** of effort, and as task hours if
  your tasks carry Remaining Work.
- **Charts**: velocity per sprint, and a burn-up with the forecast running forward.
- **Per member**: PRs split into completed / abandoned / active / draft, open
  drafts, stories and points closed, average points per story, a velocity
  sparkline, open work and how many working days it would take them at their pace.

The forecast assumes the backlog stops growing. New work pushes the date out, so
re-run it often. `-SkipTeamStats` skips the whole pass.

### Pointing it at a different repo

Edit `organization`, `project` and `repositories` in the config — or just override
them for one run:

```powershell
.\Snoop.ps1 -Organization contoso -Project Apollo -Repository web,api
```

### Choosing who to look at

Leave `people` empty and Snoop charts the 20 most active people it finds. That's the
easiest way to start on an unfamiliar repo — run it once, see who's there, then copy
the names you care about into `people`:

```json
"people": [
  "Alvarez-CONTRACTOR, Robin",
  "Bennett-CONTRACTOR, Jamie",
  "Castellano-CONTRACTOR, Alex",
  "Dunn-CONTRACTOR, Riley",
  "Emerson-CONTRACTOR, Morgan",
  "Fletcher-CONTRACTOR, Quinn"
]
```

Or filter ad hoc, without touching the config:

```powershell
.\Snoop.ps1 -People 'Alvarez-CONTRACTOR, Robin','Dunn-CONTRACTOR, Riley'
```

**Names are matched loosely.** `Alvarez-CONTRACTOR, Robin` will match a git author of
`Robin Alvarez`, a display name of `Robin M Alvarez`, or an email of
`robin.alvarez@anywhere` — matching runs on a sorted set of name tokens, with
`CONTRACTOR` stripped, rather than on the literal string. Write the names however
your org writes them and it should just work.

#### If someone's commits are missing

This is the failure worth knowing about. PRs, comments and tickets are attributed by
the **ADO identity** — the name your org holds. Commits are attributed by whatever is
in that person's **local git config**, which nobody polices. So one person can have a
row full of PRs and tickets and no commits at all, which looks exactly like "they
didn't write any code this sprint."

Snoop handles the common version of this itself: it also matches on surname plus a
first initial, so `Nick Ramirez` finds `Ramirez, Nicholas`, `Tom Whitfield` finds
`Whitfield, Thomas`, and a bare handle like `jhollister` finds `Hollister, Jordan`.
Those guesses are listed under **Matched by nickname or handle** on the dashboard —
worth a glance, because a guess can be wrong.

Anything it *can't* place shows up two ways:

- a yellow banner at the top, if the leftover identity has commits on it
- the **Identities not on the roster** table at the bottom, with a `Looks like` column
  naming the roster member it probably belongs to

Either way the fix is an alias, keyed on the git name or email:

```json
"aliases": {
  "jhollister": "Hollister, Jordan",
  "riley@personal-address.com": "Dunn-CONTRACTOR, Riley"
}
```

Aliases win over everything else, so use one whenever a guess goes wrong. If two
people on your roster would produce the same initial-and-surname guess (a Morgan Emerson
and a Mia Emerson), Snoop refuses the guess for both and waits for an alias rather
than picking one.

#### Commits on unmerged branches

By default Snoop scans every branch touched during the window, not just `main`, so
work in progress counts. This matters if your repo squashes PRs — a squash rewrites
the author onto whoever completed the PR, so the original author's commits would
otherwise vanish the moment their branch merged. Set `"includeAllBranches": false`
(or pass `-DefaultBranchOnly`) to go back to default-branch-only, which is faster.

### Several repos or clients

Keep a config per situation and pick one at startup:

```powershell
.\Snoop.ps1 -ConfigFile .\client.config.json
```

---

## Command line reference

Every switch overrides the config file for that run only.

```powershell
.\Snoop.ps1 -ConfigFile .\other.json      # use a different config
.\Snoop.ps1 -Organization contoso         # different ADO org
.\Snoop.ps1 -Project Apollo               # different project
.\Snoop.ps1 -Repository web,api           # one or more repos
.\Snoop.ps1 -People 'Dunn-CONTRACTOR, Riley'          # just these people
.\Snoop.ps1 -DaysBack 30                  # wider window
.\Snoop.ps1 -WorkItemProject Delivery     # tickets from another project
.\Snoop.ps1 -WorkItemOrganization contoso # tickets from another org
.\Snoop.ps1 -DefaultBranchOnly            # skip feature branches (faster)
.\Snoop.ps1 -AutoDiscoverPeople           # ignore the roster, chart whoever is there
.\Snoop.ps1 -IncludeWeekends              # keep empty Sat/Sun columns
.\Snoop.ps1 -SkipTeamStats                # no Teams tab (velocity, forecasts)
.\Snoop.ps1 -SkipWorkItems -SkipPrComments -SkipTeamStats # fastest possible run
.\Snoop.ps1 -OutFile C:\temp\week.html    # write somewhere else
.\Snoop.ps1 -NoOpen                       # write the file, don't launch a browser
```

Weekends are hidden unless something actually happened on them.

---

## Auth

Uses your existing `az login`. Nothing to configure.

If the target org is in a tenant your az login doesn't cover — likely for a client's
ADO — use a personal access token instead:

```powershell
$env:ADO_PAT = '<token>'
.\Snoop.ps1 -Organization theirorg -Project theirproject -Repository theirrepo
```

Create the token in ADO under **User settings → Personal access tokens**. It needs
read access to **Code**, **Pull Requests** and **Work Items**.

The token only lasts for that PowerShell window. To keep it, set it permanently:

```powershell
[Environment]::SetEnvironmentVariable('ADO_PAT', '<token>', 'User')
```

---

## Using the dashboard

- **Legend** checkboxes turn each activity type on and off.
- **View by** switches rows between people and repos.
- **Sort by** orders rows by name or by total activity.
- **Size** toggles proportional pill widths; off makes every event the same size.
- **Hide inactive rows** drops people with nothing in the window.
- Hovering a pill shows what it was — commit subject, PR title, comment text, fields changed.
- The totals table under the grid is the per-person summary.

The banner at the top tells you if Snoop ignored your roster, and why.

---

## Notes and limits

- **Merge commits** are detected from the commit message prefix, which is how ADO
  writes PR merges. A hand-written commit starting with "Merge" counts as one too.
- **PR comments** come from every open PR plus every PR closed within
  `prLookbackDays` (default 90). A comment on a PR closed longer ago is missed.
- **Ticket activity** is project-wide, not per-repo, so it stays put when you switch
  to *View by: Repo* and shows under `(work items)`.
- **A commit's day** is its author date, unless that falls outside the window and the
  commit date doesn't — which happens on rebased branches — in which case the commit
  date is used. Without that, a week of rebased work lands on nobody's row.
- **Branch scanning** only visits branches whose tip moved inside the window, capped
  at `maxBranches`. An older branch can't hold anything the default branch lacks.
- **Everything is read-only.** Snoop only issues GETs and one WIQL query. It cannot
  change anything in ADO.

## Files

| File | |
|---|---|
| `src/`, `Cargo.toml` | snoop.exe — the same fetch, match and render, in parallel, behind a local server |
| `Snoop.ps1` | the original script — fetch, match, render |
| `snoop.config.json` | your settings, created on first run |
| `snoop.config.example.json` | a blank config to copy from |
| `template.html` | the dashboard's markup, CSS and JS |
| `snoop.html` | the generated dashboard (overwritten each run) |

`snoop.config.json` and `snoop.html` are both in `.gitignore`. They hold your org,
your roster and — in the dashboard — real commit messages and PR comments, none of
which belongs in a shared repo.
