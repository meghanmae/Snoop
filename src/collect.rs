//! One refresh: fetch everything, settle who is who, and build the payload the
//! dashboard reads as DATA. The shape matches what Snoop.ps1 writes, so the
//! same template.html serves both.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anyhow::Result;
use chrono::{DateTime, Datelike, Days, Local, NaiveDate, Weekday};
use futures_util::future::join_all;
use serde_json::{Value, json};

use crate::ado::{Ado, escape, str_at, values};
use crate::config::Settings;
use crate::identity::{self, Identity, Resolver};
use crate::stats;

pub struct Event {
    pub name: String,
    pub email: String,
    pub day: NaiveDate,
    pub kind: &'static str, // commit | merge | pr | comment | ticket
    pub weight: i64,
    pub repo: String,
    pub detail: String,
    pub url: String,
}

#[derive(Clone, Copy)]
pub struct Window {
    pub today: NaiveDate,
    pub from: NaiveDate,
}

impl Window {
    fn contains(self, day: NaiveDate) -> bool {
        day >= self.from && day <= self.today
    }
}

pub fn local(value: &Value) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(value.as_str()?).ok().map(|d| d.with_timezone(&Local))
}

fn ymd(day: NaiveDate) -> String {
    day.format("%Y-%m-%d").to_string()
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() > max {
        format!("{}...", text.chars().take(max).collect::<String>())
    } else {
        text.to_string()
    }
}

fn warn(message: impl AsRef<str>) {
    println!("WARNING: {}", message.as_ref());
}

fn api_base(org: &str, project: &str) -> String {
    format!("https://dev.azure.com/{org}/{}/_apis", escape(project))
}

pub async fn refresh(ado: &Ado, s: &Settings) -> Result<Value> {
    let started = Instant::now();
    let c = &s.config;
    let today = Local::now().date_naive();
    let w = Window { today, from: today - Days::new(c.days_back.max(1) as u64 - 1) };
    println!();
    println!("Snoop  ->  {} / {}", c.organization, c.project);
    println!("Window:  {} .. {}  ({} days)", w.from.format("%a %d %b"), w.today.format("%a %d %b"), c.days_back);

    // Everything is independent until identities are resolved, so fetch it all at once.
    let (commits, prs, tickets, stats_raw) =
        tokio::join!(all_commits(ado, s, w), pr_activity(ado, s, w), ticket_events(ado, s, w), stats::fetch(ado, s, today));
    let mut events = commits?;
    events.extend(prs?);
    events.extend(tickets);
    let stats_raw = match stats_raw {
        Ok(raw) => Some(raw),
        Err(err) => {
            warn(format!("team stats skipped: {err:#}"));
            None
        }
    };

    // --- identities -> roster ---------------------------------------------

    let mut identities: Vec<Identity> = vec![];
    let mut index: HashMap<String, usize> = HashMap::new();
    for event in &events {
        let key = identity::identity_key(&event.name, &event.email);
        let i = *index.entry(key).or_insert_with(|| {
            identities.push(Identity {
                name: event.name.clone(),
                email: event.email.clone(),
                keys: identity::name_keys(&event.name, &event.email),
                count: 0,
                types: HashMap::new(),
            });
            identities.len() - 1
        });
        identities[i].count += 1;
        *identities[i].types.entry(event.kind).or_default() += 1;
    }

    let discover_mode = c.people.is_empty();
    let mut roster = identity::build_roster(&c.people);
    let (mut map, mut weak) = identity::resolve(&identities, &mut roster, &s.aliases);
    let matched = roster.iter().filter(|p| p.matched).count();

    // discovered - no roster configured, so show whoever is active here
    // fallback   - a roster matched nobody: usually the wrong org/project
    let mode = if discover_mode { "discovered" } else if matched == 0 { "fallback" } else { "roster" };
    if mode != "roster" {
        roster = identity::build_roster(&identity::discover(&identities, c.max_discovered_people));
        (map, weak) = identity::resolve(&identities, &mut roster, &s.aliases);
    }

    // Unplaced identities, with the roster member they probably are - that is
    // nearly always the alias the user needs to add.
    let mut unmatched: Vec<&Identity> = identities.iter().filter(|i| map[&i.key()] < 0).collect();
    unmatched.sort_by(|a, b| b.count.cmp(&a.count));
    let unmatched: Vec<Value> = unmatched
        .iter()
        .take(40)
        .map(|i| {
            let local_part = i.email.split('@').next().unwrap_or("");
            let mine = identity::tokens(&format!("{} {local_part}", i.name), 2);
            let suggest = roster
                .iter()
                .find(|p| identity::tokens(&p.name, 2).iter().any(|t| mine.contains(t)))
                .map(|p| p.name.clone())
                .unwrap_or_default();
            let mut types: Vec<(&&str, &usize)> = i.types.iter().collect();
            types.sort_by(|a, b| b.1.cmp(a.1));
            let types = types.iter().map(|(t, n)| format!("{n} {t}")).collect::<Vec<_>>().join(", ");
            json!({ "name": i.name, "email": i.email, "count": i.count, "types": types, "suggest": suggest })
        })
        .collect();
    weak.sort_by(|a, b| b.count.cmp(&a.count));

    // --- day columns --------------------------------------------------------

    let active: HashSet<NaiveDate> = events.iter().map(|e| e.day).collect();
    let mut days = vec![];
    let mut day = w.from;
    while day <= w.today {
        let weekend = matches!(day.weekday(), Weekday::Sat | Weekday::Sun);
        // Weekends are hidden unless something actually happened on them.
        if !weekend || c.include_weekends || active.contains(&day) {
            let label = if day == w.today { "TODAY".to_string() } else { day.format("%a %d").to_string().to_uppercase() };
            days.push(json!({ "key": ymd(day), "label": label, "weekend": weekend }));
        }
        day = day + Days::new(1);
    }

    // --- payload ------------------------------------------------------------

    let compact: Vec<Value> = events
        .iter()
        .map(|e| {
            json!({
                "p": map[&identity::identity_key(&e.name, &e.email)],
                "d": ymd(e.day), "t": e.kind, "w": e.weight, "r": e.repo, "x": e.detail, "u": e.url
            })
        })
        .collect();

    let mut resolver = Resolver::new(&map, &roster, &s.aliases);
    let stats = stats_raw.map(|raw| stats::build(raw, s, &mut resolver));

    let tickets = s.work_item_projects.iter().map(|p| format!("{}/{p}", s.work_item_org)).collect::<Vec<_>>().join(", ");
    let payload = json!({
        "meta": {
            "org": c.organization,
            "project": c.project,
            "repos": s.repos,
            "from": ymd(w.from),
            "to": ymd(w.today),
            "days": c.days_back,
            "generated": Local::now().format("%Y-%m-%d %H:%M").to_string(),
            // Tells the page a refresh has landed, even within the same minute.
            "run": Local::now().timestamp_millis(),
            "mode": mode,
            "config": s.config_name,
            "tickets": tickets,
            "branches": c.include_all_branches,
            "command": format!("snoop.exe --config {}", s.config_name),
            "seconds": started.elapsed().as_secs_f64().round(),
            "requests": ado.done.load(std::sync::atomic::Ordering::Relaxed),
        },
        "days": days,
        "people": roster.iter().enumerate().map(|(id, p)| json!({ "id": id, "name": p.name, "matched": p.matched })).collect::<Vec<_>>(),
        "events": compact,
        "unmatched": unmatched,
        "weak": weak.iter().map(|w| json!({ "name": w.name, "email": w.email, "person": w.person, "count": w.count })).collect::<Vec<_>>(),
        "stats": stats,
    });

    println!(
        "Done: {} events across {} people, {} requests in {:.1}s",
        events.len(),
        roster.len(),
        ado.done.load(std::sync::atomic::Ordering::Relaxed),
        started.elapsed().as_secs_f64()
    );
    Ok(payload)
}

// --- commits and merge commits ----------------------------------------------

const MERGE_PREFIXES: [&str; 5] =
    ["merge pull request", "merged pr", "merge branch", "merge remote-tracking", "merge commit"];

async fn commit_pages(ado: &Ado, base: &str, repo: &str, branch: Option<&str>, w: Window) -> Result<Vec<Value>> {
    let from = ymd(w.from - Days::new(1));
    let to = ymd(w.today + Days::new(1));
    let branch_query = branch
        .map(|b| format!("&searchCriteria.itemVersion.version={}&searchCriteria.itemVersion.versionType=branch", escape(b)))
        .unwrap_or_default();
    let page = 1000; // the API caps $top here, so paging is mandatory
    let mut all = vec![];
    let mut skip = 0;
    loop {
        let url = format!(
            "{base}/git/repositories/{}/commits?searchCriteria.fromDate={from}&searchCriteria.toDate={to}{branch_query}&$top={page}&$skip={skip}&api-version=7.1",
            escape(repo)
        );
        let batch = values(ado.get(&url).await?);
        let n = batch.len();
        all.extend(batch);
        skip += page;
        if n < page {
            return Ok(all);
        }
    }
}

/// Branches whose tip moved inside the window. Anything older can only hold
/// commits the default branch already has.
async fn active_branches(ado: &Ado, base: &str, repo: &str, w: Window, max: usize) -> Vec<String> {
    let url = format!("{base}/git/repositories/{}/stats/branches?api-version=7.1", escape(repo));
    let stats = match ado.get(&url).await {
        Ok(v) => values(v),
        Err(err) => {
            warn(format!("branch list for {repo} - {err:#}"));
            return vec![];
        }
    };
    let cutoff = w.from - Days::new(1);
    let mut recent: Vec<(DateTime<Local>, String)> = stats
        .iter()
        .filter_map(|b| {
            let when = local(&b["commit"]["committer"]["date"])?;
            (when.date_naive() >= cutoff).then(|| (when, str_at(b, &["name"]).to_string()))
        })
        .collect();
    recent.sort_by(|a, b| b.0.cmp(&a.0));
    recent.into_iter().take(max).map(|(_, name)| name).collect()
}

async fn repo_commits(ado: &Ado, s: &Settings, w: Window, repo: &str) -> Result<Vec<Event>> {
    let c = &s.config;
    let base = api_base(&c.organization, &c.project);

    // Feature branches are where in-progress work lives, and a squash merge
    // rewrites the author onto whoever completed the PR.
    let branches_fut = async {
        if c.include_all_branches { active_branches(ado, &base, repo, w, c.max_branches).await } else { vec![] }
    };
    let (default, branches) = tokio::join!(commit_pages(ado, &base, repo, None, w), branches_fut);
    let mut lists = vec![default?];
    let results = join_all(branches.iter().map(|b| commit_pages(ado, &base, repo, Some(b), w))).await;
    for (branch, result) in branches.iter().zip(results) {
        match result {
            Ok(list) => lists.push(list),
            Err(err) => warn(format!("branch '{branch}' in {repo} - {err:#}")),
        }
    }

    let mut seen = HashSet::new();
    let mut events = vec![];
    let mut scanned = 0;
    for commit in lists.iter().flatten() {
        scanned += 1;
        let id = str_at(commit, &["commitId"]);
        if !id.is_empty() && !seen.insert(id.to_string()) {
            continue;
        }

        // ADO hands back a commit if EITHER date lands in the range, so a
        // rebased branch can arrive with an author date well outside the
        // window. Chart it on whichever date is actually in it.
        let mut when = local(&commit["author"]["date"]);
        if when.is_none_or(|d| !w.contains(d.date_naive())) {
            if let Some(committed) = local(&commit["committer"]["date"]).filter(|d| w.contains(d.date_naive())) {
                when = Some(committed);
            }
        }
        let Some(when) = when else { continue };
        if !w.contains(when.date_naive()) {
            continue;
        }

        let counts = &commit["changeCounts"];
        let files = ["Add", "Edit", "Delete"].iter().map(|k| counts[*k].as_i64().unwrap_or(0)).sum::<i64>();
        let comment = str_at(commit, &["comment"]);
        let lower = comment.to_lowercase();
        let is_merge = MERGE_PREFIXES.iter().any(|p| lower.starts_with(p));
        let subject = truncate(comment.lines().next().unwrap_or("").trim_end(), 90);

        events.push(Event {
            name: str_at(commit, &["author", "name"]).to_string(),
            email: str_at(commit, &["author", "email"]).to_string(),
            day: when.date_naive(),
            kind: if is_merge { "merge" } else { "commit" },
            weight: files,
            repo: repo.to_string(),
            detail: format!("{subject}  ({files} files changed)"),
            url: str_at(commit, &["remoteUrl"]).to_string(),
        });
    }
    let suffix = if branches.is_empty() { String::new() } else { format!(" (+{} active branches)", branches.len()) };
    println!("Commits: {repo}  {scanned} scanned{suffix}, {} in window", events.len());
    Ok(events)
}

async fn all_commits(ado: &Ado, s: &Settings, w: Window) -> Result<Vec<Event>> {
    let results = join_all(s.repos.iter().map(|repo| repo_commits(ado, s, w, repo))).await;
    let mut events = vec![];
    for result in results {
        events.extend(result?);
    }
    Ok(events)
}

// --- pull requests and PR comments ------------------------------------------

pub async fn pr_pages(ado: &Ado, base: &str, repo: &str, query: &str) -> Result<Vec<Value>> {
    let page = 500;
    let mut all = vec![];
    let mut skip = 0;
    loop {
        let url = format!(
            "{base}/git/repositories/{}/pullrequests?{query}&$top={page}&$skip={skip}&api-version=7.1",
            escape(repo)
        );
        let batch = values(ado.get(&url).await?);
        let n = batch.len();
        all.extend(batch);
        skip += page;
        if n < page {
            return Ok(all);
        }
    }
}

async fn pr_activity(ado: &Ado, s: &Settings, w: Window) -> Result<Vec<Event>> {
    let c = &s.config;
    let base = api_base(&c.organization, &c.project);
    let lookback = ymd(w.from - Days::new(c.pr_lookback_days.max(0) as u64));
    let to = ymd(w.today + Days::new(1));

    // Everything open, plus everything closed inside the lookback: exactly the
    // set that can hold a comment inside the window.
    let closed_query =
        format!("searchCriteria.status=all&searchCriteria.queryTimeRangeType=closed&searchCriteria.minTime={lookback}&searchCriteria.maxTime={to}");
    let lists = join_all(s.repos.iter().map(|repo| {
        let base = &base;
        let closed_query = &closed_query;
        async move {
            let (open, closed) = tokio::join!(
                pr_pages(ado, base, repo, "searchCriteria.status=active"),
                pr_pages(ado, base, repo, closed_query)
            );
            let mut prs: Vec<Value> = open?;
            let mut ids: HashSet<i64> = prs.iter().filter_map(|p| p["pullRequestId"].as_i64()).collect();
            for pr in closed? {
                if ids.insert(pr["pullRequestId"].as_i64().unwrap_or(-1)) {
                    prs.push(pr);
                }
            }
            println!("Pull requests: {repo}  {}", prs.len());
            anyhow::Ok(prs.into_iter().map(|pr| (repo.clone(), pr)).collect::<Vec<_>>())
        }
    }))
    .await;
    let mut prs = vec![];
    for list in lists {
        prs.extend(list?);
    }

    let mut events = vec![];
    let mut push = |event: Event| {
        if w.contains(event.day) {
            events.push(event);
        }
    };

    for (repo, pr) in &prs {
        let id = pr["pullRequestId"].as_i64().unwrap_or(0);
        let title = str_at(pr, &["title"]);
        if let Some(created) = local(&pr["creationDate"]) {
            push(Event {
                name: str_at(pr, &["createdBy", "displayName"]).into(),
                email: str_at(pr, &["createdBy", "uniqueName"]).into(),
                day: created.date_naive(),
                kind: "pr",
                weight: 20,
                repo: repo.clone(),
                detail: format!("Opened PR {id}: {title}"),
                url: str_at(pr, &["url"]).into(),
            });
        }
        if let Some(closed) = local(&pr["closedDate"]) {
            let closer = if pr["closedBy"].is_object() { &pr["closedBy"] } else { &pr["createdBy"] };
            let verb = if str_at(pr, &["status"]) == "abandoned" { "Abandoned" } else { "Completed" };
            push(Event {
                name: str_at(closer, &["displayName"]).into(),
                email: str_at(closer, &["uniqueName"]).into(),
                day: closed.date_naive(),
                kind: "pr",
                weight: 20,
                repo: repo.clone(),
                detail: format!("{verb} PR {id}: {title}"),
                url: str_at(pr, &["url"]).into(),
            });
        }
    }

    let threads = join_all(prs.iter().map(|(repo, pr)| {
        let url = format!(
            "{base}/git/repositories/{}/pullRequests/{}/threads?api-version=7.1",
            escape(repo),
            pr["pullRequestId"].as_i64().unwrap_or(0)
        );
        async move { ado.get(&url).await }
    }))
    .await;

    let mut comments = 0;
    for ((repo, pr), result) in prs.iter().zip(threads) {
        let threads = match result {
            Ok(v) => values(v),
            Err(err) => {
                warn(format!("threads for PR {}: {err:#}", pr["pullRequestId"]));
                continue;
            }
        };
        for comment in threads.iter().filter_map(|t| t["comments"].as_array()).flatten() {
            if str_at(comment, &["commentType"]) == "system" || comment["isDeleted"].as_bool() == Some(true) {
                continue;
            }
            let Some(when) = local(&comment["publishedDate"]) else { continue };
            let content = str_at(comment, &["content"]);
            let text = truncate(&content.split_whitespace().collect::<Vec<_>>().join(" "), 90);
            comments += 1;
            push(Event {
                name: str_at(comment, &["author", "displayName"]).into(),
                email: str_at(comment, &["author", "uniqueName"]).into(),
                day: when.date_naive(),
                kind: "comment",
                weight: content.chars().count() as i64,
                repo: repo.clone(),
                detail: format!("PR {}: {text}", pr["pullRequestId"]),
                url: str_at(pr, &["url"]).into(),
            });
        }
    }
    println!("PR comments: {comments} scanned");
    Ok(events)
}

// --- work item (ticket) activity --------------------------------------------

const NOISE_FIELDS: [&str; 9] = [
    "System.Rev",
    "System.ChangedDate",
    "System.ChangedBy",
    "System.AuthorizedAs",
    "System.AuthorizedDate",
    "System.RevisedDate",
    "System.Watermark",
    "System.PersonId",
    "System.CommentCount",
];

pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

pub fn area_clause(paths: &[String]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let clauses: Vec<String> = paths.iter().map(|p| format!("[System.AreaPath] UNDER {}", quote(p))).collect();
    format!(" AND ({})", clauses.join(" OR "))
}

pub async fn wiql_ids(ado: &Ado, base: &str, query: &str) -> Result<Vec<i64>> {
    let result = ado.post(&format!("{base}/wit/wiql?api-version=7.1"), &json!({ "query": query })).await?;
    Ok(result["workItems"].as_array().map(|a| a.iter().filter_map(|w| w["id"].as_i64()).collect()).unwrap_or_default())
}

async fn ticket_events(ado: &Ado, s: &Settings, w: Window) -> Vec<Event> {
    let c = &s.config;
    let mut events = vec![];
    for project in &s.work_item_projects {
        let base = api_base(&s.work_item_org, project);
        // A shared board can hold thousands of items unrelated to this team,
        // so narrow to the area paths they actually work under.
        let query = format!(
            "SELECT [System.Id] FROM WorkItems WHERE [System.TeamProject] = {} AND [System.ChangedDate] >= '{}'{} ORDER BY [System.ChangedDate] DESC",
            quote(project),
            ymd(w.from),
            area_clause(&c.work_items.area_paths)
        );
        let ids = match wiql_ids(ado, &base, &query).await {
            Ok(ids) => ids,
            Err(err) => {
                warn(format!("work item query against {}/{project} failed: {err:#}", s.work_item_org));
                continue;
            }
        };

        let updates = join_all(ids.iter().map(|id| {
            let url = format!("{base}/wit/workitems/{id}/updates?api-version=7.1");
            async move { ado.get(&url).await }
        }))
        .await;

        let before = events.len();
        for (id, result) in ids.iter().zip(updates) {
            let Ok(updates) = result else { continue };
            for update in values(updates) {
                if !update["revisedBy"].is_object() {
                    continue;
                }
                let stamp = update["fields"]["System.ChangedDate"]["newValue"].clone();
                let when = local(&stamp).or_else(|| local(&update["revisedDate"]));
                // ADO writes a year-9999 sentinel on some revisions.
                let Some(when) = when.filter(|d| d.year() < 9000) else { continue };

                let changed: Vec<&str> = update["fields"]
                    .as_object()
                    .map(|f| f.keys().map(String::as_str).filter(|k| !NOISE_FIELDS.contains(k)).collect())
                    .unwrap_or_default();
                let relations = !update["relations"].is_null();
                if changed.is_empty() && !relations {
                    continue;
                }
                let summary = if changed.is_empty() {
                    "links".to_string()
                } else {
                    truncate(&changed.iter().map(|f| f.rsplit('.').next().unwrap_or(f)).collect::<Vec<_>>().join(", "), 80)
                };
                let day = when.date_naive();
                if !w.contains(day) {
                    continue;
                }
                events.push(Event {
                    name: str_at(&update, &["revisedBy", "displayName"]).into(),
                    email: str_at(&update, &["revisedBy", "uniqueName"]).into(),
                    day,
                    kind: "ticket",
                    weight: changed.len().max(1) as i64,
                    repo: "(work items)".into(),
                    detail: format!("#{id}  {summary}"),
                    url: format!("https://dev.azure.com/{}/{}/_workitems/edit/{id}", s.work_item_org, escape(project)),
                });
            }
        }
        println!("Work items: {}/{project}  {} changed, {} revisions in window", s.work_item_org, ids.len(), events.len() - before);
    }
    events
}
