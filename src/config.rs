//! snoop.config.json: what to look at. Re-read on every refresh, so edits to the
//! file show up on the next press of Refresh without restarting.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Config {
    pub organization: String,
    pub project: String,
    /// Legacy: repos outside any team. Repos now come from the enabled teams;
    /// the settings page moves anything listed here into a team.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<String>,
    pub days_back: i64,
    pub people: Vec<String>,
    pub aliases: IndexMap<String, String>,
    pub work_items: WorkItemConfig,
    pub max_discovered_people: usize,
    pub pr_lookback_days: i64,
    pub include_all_branches: bool,
    pub max_branches: usize,
    pub include_weekends: bool,
    /// Team name -> its repos, or { repositories, areaPaths, members, enabled }.
    /// The enabled teams' repos are the repos Snoop watches.
    pub teams: IndexMap<String, TeamConfig>,
    pub stats: StatsConfig,
    /// How many Azure DevOps requests may be in flight at once.
    pub max_concurrent_requests: usize,
    /// Local port for the dashboard. Falls back to any free port if taken.
    pub port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // Blank until the settings page fills them in.
            organization: String::new(),
            project: String::new(),
            repositories: vec![],
            days_back: 14,
            people: vec![],
            aliases: IndexMap::new(),
            work_items: WorkItemConfig::default(),
            max_discovered_people: 20,
            pr_lookback_days: 90,
            include_all_branches: true,
            max_branches: 50,
            include_weekends: false,
            teams: IndexMap::new(),
            stats: StatsConfig::default(),
            max_concurrent_requests: 12,
            port: 7414,
        }
    }
}

/// Tickets do not always live with the code. Blank fields mean "same as the
/// repo"; fill them in when the boards are in another project or org.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct WorkItemConfig {
    pub organization: String,
    pub projects: Vec<String>,
    pub area_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TeamConfig {
    Repos(Vec<String>),
    Repo(String),
    Detail(TeamDetail),
}

impl TeamConfig {
    pub fn detail(&self) -> TeamDetail {
        match self.clone() {
            TeamConfig::Repos(repositories) => TeamDetail { repositories, ..TeamDetail::default() },
            TeamConfig::Repo(repo) => TeamDetail { repositories: vec![repo], ..TeamDetail::default() },
            TeamConfig::Detail(detail) => detail,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TeamDetail {
    /// A disabled team stays in the config but is left out of everything.
    #[serde(skip_serializing_if = "is_true")]
    pub enabled: bool,
    pub repositories: Vec<String>,
    /// Otherwise the backlog is whatever the members are assigned.
    pub area_paths: Vec<String>,
    /// Stories titled "SCH - ...", "SCH: ..." or "[SCH] ..." belong to the team,
    /// for teams that share one backlog and tell their stories apart by name.
    pub title_prefixes: Vec<String>,
    /// Otherwise anyone with a PR in the team's repos.
    pub members: Vec<String>,
}

impl Default for TeamDetail {
    fn default() -> Self {
        Self { enabled: true, repositories: vec![], area_paths: vec![], title_prefixes: vec![], members: vec![] }
    }
}

fn is_true(value: &bool) -> bool {
    *value
}

/// The Teams view reaches back much further than daysBack: velocity and
/// forecasts need months of closed stories, not a fortnight.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StatsConfig {
    pub months_back: u32,
    pub sprint_length_days: i64,
    /// Any sprint's first day, to line up with real sprints.
    pub sprint_anchor_date: String,
    /// How many recent sprints the forecast averages.
    pub velocity_sprints: usize,
    pub story_types: Vec<String>,
    /// Converts task Remaining Work hours into dev-days.
    pub hours_per_dev_day: f64,
}

impl Default for StatsConfig {
    fn default() -> Self {
        Self {
            months_back: 6,
            sprint_length_days: 14,
            sprint_anchor_date: String::new(),
            velocity_sprints: 6,
            story_types: vec!["User Story".into(), "Product Backlog Item".into(), "Story".into()],
            hours_per_dev_day: 6.0,
        }
    }
}

pub struct Team {
    pub name: String,
    pub repos: Vec<String>,
    pub area_paths: Vec<String>,
    pub title_prefixes: Vec<String>,
    pub members: Vec<String>,
}

/// The config with every "blank means the same as..." rule applied.
pub struct Settings {
    pub config: Config,
    pub config_name: String,
    /// Every repo to watch: the enabled teams' repos, plus any legacy ones.
    pub repos: Vec<String>,
    pub work_item_org: String,
    pub work_item_projects: Vec<String>,
    /// Lower-cased keys: alias lookups ignore case.
    pub aliases: HashMap<String, String>,
    /// Enabled teams only.
    pub teams: Vec<Team>,
}

/// --config wins; then snoop.config.json in the current directory; then next
/// to the exe, where a starter file is written if there is none anywhere.
pub fn locate(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let path = match explicit {
        Some(path) => path,
        None if Path::new("snoop.config.json").exists() => PathBuf::from("snoop.config.json"),
        None => std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("snoop.config.json"),
    };
    if !path.exists() {
        write(&path, &Config::default()).context("could not write a starter config")?;
        println!("Created starter config: {}", path.display());
    }
    Ok(path)
}

pub fn read(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).with_context(|| format!("could not parse {}", path.display()))
}

pub fn write(path: &Path, config: &Config) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(config)?).with_context(|| format!("could not write {}", path.display()))
}

impl Config {
    /// What still has to be filled in before a fetch can work, if anything.
    pub fn missing(&self) -> Option<&'static str> {
        if self.organization.trim().is_empty() {
            Some("an organization")
        } else if self.project.trim().is_empty() {
            Some("a project")
        } else if self.repos().is_empty() {
            Some("at least one active team with a repository")
        } else {
            None
        }
    }

    pub fn enabled_teams(&self) -> impl Iterator<Item = (&String, TeamDetail)> {
        self.teams.iter().map(|(name, team)| (name, team.detail())).filter(|(_, team)| team.enabled)
    }

    /// Legacy repos first, then each enabled team's, without repeats.
    pub fn repos(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        let legacy = self.repositories.iter().cloned();
        let teams = self.enabled_teams().flat_map(|(_, team)| team.repositories);
        legacy
            .chain(teams)
            .map(|repo| repo.trim().to_string())
            .filter(|repo| !repo.is_empty() && seen.insert(repo.to_lowercase()))
            .collect()
    }
}

pub fn load(path: &Path) -> Result<Settings> {
    let config = read(path)?;

    let work_item_org = if config.work_items.organization.is_empty() {
        config.organization.clone()
    } else {
        config.work_items.organization.clone()
    };
    let work_item_projects = if config.work_items.projects.is_empty() {
        vec![config.project.clone()]
    } else {
        config.work_items.projects.clone()
    };
    let aliases = config.aliases.iter().map(|(k, v)| (k.to_lowercase(), v.clone())).collect();

    let teams = config
        .enabled_teams()
        .map(|(name, detail)| Team {
            name: name.clone(),
            repos: detail.repositories,
            area_paths: detail.area_paths,
            title_prefixes: detail.title_prefixes,
            members: detail.members,
        })
        .collect();

    Ok(Settings {
        config_name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        repos: config.repos(),
        config,
        work_item_org,
        work_item_projects,
        aliases,
        teams,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Config {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn repos_come_from_active_teams_and_legacy_list() {
        let config = parse(
            r#"{
                "organization": "o", "project": "p",
                "repositories": ["web", "API"],
                "teams": {
                    "A": ["api", "x"],
                    "B": { "repositories": ["y"], "enabled": false },
                    "C": { "repositories": ["z", "X"] },
                    "D": "solo"
                }
            }"#,
        );
        assert_eq!(config.repos(), ["web", "API", "x", "z", "solo"]);
        assert_eq!(config.enabled_teams().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), ["A", "C", "D"]);
        assert_eq!(config.missing(), None);
    }

    #[test]
    fn example_config_reads() {
        let config = parse(include_str!("../snoop.config.example.json"));
        assert_eq!(config.missing(), None);
        assert!(!config.repos().iter().any(|r| r == "Contoso.Web.Legacy"));
    }

    #[test]
    fn needs_an_active_team_with_a_repo() {
        let config = parse(r#"{ "organization": "o", "project": "p", "teams": { "A": { "repositories": ["x"], "enabled": false } } }"#);
        assert!(config.repos().is_empty());
        assert_eq!(config.missing(), Some("at least one active team with a repository"));
    }

    #[test]
    fn writes_enabled_and_repositories_only_when_they_matter() {
        let config = parse(
            r#"{ "organization": "o", "project": "p", "teams": { "On": { "repositories": ["x"] }, "Off": { "repositories": ["y"], "enabled": false } } }"#,
        );
        let json = serde_json::to_value(&config).unwrap();
        assert!(json.get("repositories").is_none());
        assert!(json["teams"]["On"].get("enabled").is_none());
        assert_eq!(json["teams"]["Off"]["enabled"], false);
        assert_eq!(parse(&json.to_string()).repos(), ["x"]);
    }
}
