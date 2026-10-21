//! Team stats: the longer window behind the Teams view. Pulls every PR created
//! since stats.monthsBack (plus any still open) and every story/task still open
//! or closed since then. The page does the arithmetic; this only ships the
//! records, tied to roster ids.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use chrono::{Local, Months, NaiveDate, TimeZone};
use futures_util::future::join_all;
use serde_json::{Value, json};

use crate::ado::{Ado, escape, str_at, values};
use crate::collect::{area_clause, local, pr_pages, quote, wiql_ids};
use crate::config::Settings;
use crate::identity::Resolver;

pub struct Raw {
    from: NaiveDate,
    today: NaiveDate,
    prs: Vec<(String, Value)>,
    items: Vec<Item>,
    /// Every state seen, in ADO's workflow order, with its category.
    states: Vec<(String, String)>,
}

struct Item {
    id: i64,
    /// Index into Settings.work_item_projects.
    project: usize,
    fields: Value,
    category: String,
    closed: Option<NaiveDate>,
    kind: Kind,
    parent: Option<i64>,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Story,
    Bug,
    Task,
}

impl Kind {
    fn of(work_item_type: &str) -> Kind {
        if work_item_type.eq_ignore_ascii_case("Task") {
            Kind::Task
        } else if work_item_type.eq_ignore_ascii_case("Bug") {
            Kind::Bug
        } else {
            Kind::Story
        }
    }

    fn key(self) -> &'static str {
        match self {
            Kind::Story => "story",
            Kind::Bug => "bug",
            Kind::Task => "task",
        }
    }
}

/// Only stories carry points. Bugs and tasks are fetched as their own kinds,
/// and features and epics sit above stories, so their points would count the
/// same work twice.
fn query_types(story_types: &[String]) -> Vec<String> {
    let not_stories = ["Task", "Bug", "Feature", "Epic"];
    let mut types: Vec<String> = vec![];
    for t in story_types {
        let t = t.trim();
        if !t.is_empty() && !not_stories.iter().any(|n| n.eq_ignore_ascii_case(t)) && !types.iter().any(|x| x.eq_ignore_ascii_case(t)) {
            types.push(t.to_string());
        }
    }
    types.push("Bug".into());
    types.push("Task".into());
    types
}

/// State names differ per process (Done vs Closed, Removed vs Cut). Used only
/// when ADO can't say which category a state belongs to.
fn fallback_category(state: &str) -> &'static str {
    let is = |names: &[&str]| names.iter().any(|n| n.eq_ignore_ascii_case(state));
    if is(&["Closed", "Done", "Completed"]) {
        "Completed"
    } else if is(&["Removed", "Cut"]) {
        "Removed"
    } else if is(&["Resolved"]) {
        "Resolved"
    } else if is(&["New", "Proposed", "To Do", "Approved", "Design"]) {
        "Proposed"
    } else {
        "InProgress"
    }
}

pub async fn fetch(ado: &Ado, s: &Settings, today: NaiveDate) -> Result<Raw> {
    let c = &s.config;
    let from = today.checked_sub_months(Months::new(c.stats.months_back)).unwrap_or(today);
    let from_iso = Local
        .from_local_datetime(&from.and_hms_opt(0, 0, 0).unwrap_or_default())
        .earliest()
        .map(|d| d.to_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_default();

    let (prs, items) = tokio::join!(fetch_prs(ado, s, &from_iso), fetch_items(ado, s, from));
    let (items, states) = items?;
    let raw = Raw { from, today, prs, items, states };
    println!("Team stats since {from}: {} PRs, {} stories/bugs/tasks", raw.prs.len(), raw.items.len());
    Ok(raw)
}

/// Everything created in the window, plus anything still open, so a draft that
/// has sat for a year still shows up as a draft.
async fn fetch_prs(ado: &Ado, s: &Settings, from_iso: &str) -> Vec<(String, Value)> {
    let c = &s.config;
    let base = format!("https://dev.azure.com/{}/{}/_apis", c.organization, escape(&c.project));

    let created = format!("searchCriteria.status=all&searchCriteria.queryTimeRangeType=created&searchCriteria.minTime={from_iso}");
    let lists = join_all(s.repos.iter().map(|repo| {
        let (base, created) = (&base, &created);
        async move {
            let (recent, open) =
                tokio::join!(pr_pages(ado, base, repo, created), pr_pages(ado, base, repo, "searchCriteria.status=active"));
            let mut out = vec![];
            let mut ids = HashSet::new();
            for result in [recent, open] {
                match result {
                    Ok(list) => {
                        for pr in list {
                            if ids.insert(pr["pullRequestId"].as_i64().unwrap_or(-1)) {
                                out.push((repo.clone(), pr));
                            }
                        }
                    }
                    Err(err) => println!("WARNING: PRs for {repo} - {err:#}"),
                }
            }
            out
        }
    }))
    .await;
    lists.into_iter().flatten().collect()
}

async fn fetch_items(ado: &Ado, s: &Settings, from: NaiveDate) -> Result<(Vec<Item>, Vec<(String, String)>)> {
    let c = &s.config;
    let types = query_types(&c.stats.story_types);
    let mut state_order: Vec<(String, String)> = vec![];

    // Narrow to the configured area paths, plus any team's own - a team's area
    // path need not sit under the global one.
    let mut areas = vec![];
    if !c.work_items.area_paths.is_empty() {
        let mut seen = HashSet::new();
        for path in c.work_items.area_paths.iter().chain(s.teams.iter().flat_map(|t| t.area_paths.iter())) {
            if seen.insert(path.to_lowercase()) {
                areas.push(path.clone());
            }
        }
    }

    let mut items = vec![];
    for (project_index, project) in s.work_item_projects.iter().enumerate() {
        let base = format!("https://dev.azure.com/{}/{}/_apis", s.work_item_org, escape(project));

        // Ask ADO which category each state belongs to rather than hard-coding names.
        let states = join_all(types.iter().map(|t| {
            let url = format!("{base}/wit/workitemtypes/{}/states?api-version=7.1", escape(t));
            async move { (t.clone(), ado.get(&url).await) }
        }))
        .await;
        let mut categories: HashMap<(String, String), String> = HashMap::new();
        for (kind, result) in states {
            // An error means the type isn't in this process - fine.
            for state in result.map(values).unwrap_or_default() {
                let (name, category) = (str_at(&state, &["name"]).to_string(), str_at(&state, &["category"]).to_string());
                if !state_order.iter().any(|(n, _)| n.eq_ignore_ascii_case(&name)) {
                    state_order.push((name.clone(), category.clone()));
                }
                categories.insert((kind.clone(), name), category);
            }
        }
        let names_in = |category: &str, defaults: &[&str]| {
            let mut names: Vec<String> = defaults.iter().map(|s| s.to_string()).collect();
            for ((_, state), cat) in &categories {
                if cat == category && !names.contains(state) {
                    names.push(state.clone());
                }
            }
            names.iter().map(|n| quote(n)).collect::<Vec<_>>().join(", ")
        };

        let query = format!(
            "SELECT [System.Id] FROM WorkItems WHERE [System.TeamProject] = {} AND [System.WorkItemType] IN ({}) \
             AND [System.State] NOT IN ({}) AND ([System.State] NOT IN ({}) OR [System.ChangedDate] >= '{}'){}",
            quote(project),
            types.iter().map(|t| quote(t)).collect::<Vec<_>>().join(", "),
            names_in("Removed", &["Removed"]),
            names_in("Completed", &["Closed", "Done"]),
            from.format("%Y-%m-%d"),
            area_clause(&areas)
        );
        let ids = match wiql_ids(ado, &base, &query).await {
            Ok(ids) => ids,
            Err(err) => {
                println!("WARNING: stats query against {project} failed: {err:#}");
                println!("         On a big shared project, set workItems.areaPaths (or a team's areaPaths) to narrow it.");
                continue;
            }
        };

        // WIQL only returns ids; pull the fields in batches of 200.
        let batches = join_all(ids.chunks(200).map(|chunk| {
            let url = format!("{base}/wit/workitemsbatch?api-version=7.1");
            // The batch call leaves System.Parent out; relations carry it instead.
            let body = json!({ "ids": chunk, "$expand": "relations" });
            async move { ado.post(&url, &body).await }
        }))
        .await;

        for batch in batches {
            for mut item in values(batch?) {
                let fields = item["fields"].take();
                let kind = str_at(&fields, &["System.WorkItemType"]).to_string();
                let state = str_at(&fields, &["System.State"]).to_string();
                let category = categories
                    .get(&(kind.clone(), state.clone()))
                    .cloned()
                    .unwrap_or_else(|| fallback_category(&state).to_string());
                if category == "Removed" {
                    continue;
                }

                let mut closed = None;
                if category == "Completed" {
                    // Changed in the window isn't closed in the window.
                    let date = ["Microsoft.VSTS.Common.ClosedDate", "Microsoft.VSTS.Common.StateChangeDate", "System.ChangedDate"]
                        .iter()
                        .find_map(|f| local(&fields[*f]))
                        .map(|d| d.date_naive());
                    match date {
                        Some(day) if day >= from => closed = Some(day),
                        _ => continue,
                    }
                }
                let parent = item["relations"].as_array().and_then(|links| {
                    links
                        .iter()
                        .find(|l| str_at(l, &["rel"]) == "System.LinkTypes.Hierarchy-Reverse")
                        .and_then(|l| str_at(l, &["url"]).rsplit('/').next()?.parse().ok())
                });
                items.push(Item {
                    id: item["id"].as_i64().unwrap_or(0),
                    project: project_index,
                    fields,
                    category,
                    closed,
                    kind: Kind::of(&kind),
                    parent,
                });
            }
        }
        println!("Stories, bugs and tasks: {}/{project}  {}", s.work_item_org, ids.len());
    }
    Ok((items, state_order))
}

pub fn build(raw: Raw, s: &Settings, resolver: &mut Resolver<'_>) -> Value {
    let st = &s.config.stats;

    let prs: Vec<Value> = raw
        .prs
        .iter()
        .filter_map(|(repo, pr)| {
            let person = resolver.find(str_at(pr, &["createdBy", "displayName"]), str_at(pr, &["createdBy", "uniqueName"]));
            if person < 0 {
                return None;
            }
            let mut status = str_at(pr, &["status"]);
            if status == "active" && pr["isDraft"].as_bool() == Some(true) {
                status = "draft";
            }
            let day = |key: &str| local(&pr[key]).map(|d| d.date_naive().format("%Y-%m-%d").to_string()).unwrap_or_default();
            Some(json!({ "p": person, "r": repo, "s": status, "c": day("creationDate"), "x": day("closedDate") }))
        })
        .collect();

    let items: Vec<Value> = raw
        .items
        .iter()
        .map(|item| {
            let f = &item.fields;
            // Agile/CMMI use StoryPoints, Scrum uses Effort, CMMI requirements use Size.
            let points = ["Microsoft.VSTS.Scheduling.StoryPoints", "Microsoft.VSTS.Scheduling.Effort", "Microsoft.VSTS.Scheduling.Size"]
                .iter()
                .find_map(|k| f[*k].as_f64());
            let points = if item.kind == Kind::Story { points } else { None };
            let remaining = if item.kind == Kind::Task { f["Microsoft.VSTS.Scheduling.RemainingWork"].as_f64() } else { None };
            let assigned = &f["System.AssignedTo"];
            let person = if assigned.is_object() {
                resolver.find(str_at(assigned, &["displayName"]), str_at(assigned, &["uniqueName"]))
            } else {
                -1
            };
            json!({
                "id": item.id,
                "pj": item.project,
                "p": person,
                "k": item.kind.key(),
                "s": str_at(f, &["System.State"]),
                "g": item.category,
                "pts": points,
                "rem": remaining,
                "a": str_at(f, &["System.AreaPath"]),
                "t": str_at(f, &["System.Title"]),
                // A task or bug's team is its story's, since they rarely carry the title prefix.
                "par": item.parent,
                "c": item.closed.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
            })
        })
        .collect();

    // No teams configured means one team made of everyone on the roster.
    let everyone;
    let teams: Vec<(&str, &[String], &[String], &[String], Option<&[String]>)> = if s.teams.is_empty() {
        everyone = resolver.roster.iter().map(|p| p.name.clone()).collect::<Vec<_>>();
        vec![("Everyone", &s.repos[..], &[][..], &[][..], Some(&everyone[..]))]
    } else {
        s.teams
            .iter()
            .map(|t| {
                let members = (!t.members.is_empty()).then_some(&t.members[..]);
                (t.name.as_str(), &t.repos[..], &t.area_paths[..], &t.title_prefixes[..], members)
            })
            .collect()
    };
    let teams: Vec<Value> = teams
        .into_iter()
        .map(|(name, repos, area_paths, title_prefixes, members)| {
            // null tells the page to work the members out from who has PRs in the repos.
            let members = members.map(|names| {
                names
                    .iter()
                    .filter_map(|member| {
                        let id = resolver.find(member, "");
                        if id < 0 {
                            println!("WARNING: team '{name}': member '{member}' is not on the roster");
                        }
                        (id >= 0).then_some(id)
                    })
                    .collect::<Vec<_>>()
            });
            json!({ "name": name, "repos": repos, "areaPaths": area_paths, "titlePrefixes": title_prefixes, "members": members })
        })
        .collect();

    json!({
        "from": raw.from.format("%Y-%m-%d").to_string(),
        "to": raw.today.format("%Y-%m-%d").to_string(),
        "sprintDays": st.sprint_length_days,
        "anchor": st.sprint_anchor_date,
        "velocitySprints": st.velocity_sprints,
        "hoursPerDay": st.hours_per_dev_day,
        "teams": teams,
        "prs": prs,
        "items": items,
        "states": raw.states.iter().map(|(n, g)| json!({ "n": n, "g": g })).collect::<Vec<_>>(),
        // For links that open a set of items as a query in Azure DevOps.
        "wiOrg": s.work_item_org,
        "wiProjects": s.work_item_projects,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_stories_count_as_stories() {
        let types = query_types(&["User Story".into(), "Feature".into(), "bug".into(), "Epic".into(), "Product Backlog Item".into()]);
        assert_eq!(types, ["User Story", "Product Backlog Item", "Bug", "Task"]);
        assert!(Kind::of("Bug") == Kind::Bug && Kind::of("Task") == Kind::Task && Kind::of("User Story") == Kind::Story);
    }
}
