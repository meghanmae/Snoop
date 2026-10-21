//! Who is who. Commits carry whatever name sits in someone's local git config;
//! PRs and tickets carry the name ADO knows them by. This collapses both onto
//! one roster row per person.

use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Grade {
    /// Built from the whole name. Certain.
    Strong,
    /// Surname plus a first initial ("nramirez"). Catches nicknames and bare
    /// handles, but can collide between two people.
    Weak,
}

pub type Keys = HashMap<String, Grade>;

/// Lower-cased name tokens with "CONTRACTOR" and punctuation stripped.
pub fn tokens(text: &str, min_len: usize) -> Vec<String> {
    let clean: String = text
        .to_lowercase()
        .replace("contractor", " ")
        .chars()
        .map(|c| if c.is_ascii_lowercase() { c } else { ' ' })
        .collect();
    clean.split_whitespace().filter(|t| t.len() > min_len).map(str::to_string).collect()
}

fn add_key(keys: &mut Keys, key: String, grade: Grade) {
    if key.len() < 4 {
        return; // too short to mean anything
    }
    if grade == Grade::Strong || !keys.contains_key(&key) {
        keys.insert(key, grade);
    }
}

fn sorted_join(parts: &[&str]) -> String {
    let mut parts = parts.to_vec();
    parts.sort_unstable();
    parts.concat()
}

/// Reduce a name/email to canonical keys, so "Alvarez-CONTRACTOR, Robin",
/// "Robin Alvarez" and "robin.alvarez@x.com" all land on the same key.
pub fn name_keys(name: &str, email: &str) -> Keys {
    let mut keys = Keys::new();
    let mut sources = vec![];
    if !name.is_empty() {
        sources.push(name);
    }
    if !email.is_empty() {
        sources.push(email.split('@').next().unwrap_or(""));
    }

    for src in sources {
        let tokens = tokens(src, 1);
        let parts: Vec<&str> = tokens.iter().map(String::as_str).collect();
        match parts.len() {
            0 => continue,
            1 => {
                // A bare handle - "jhollister". Only a weak key is possible.
                add_key(&mut keys, parts[0].to_string(), Grade::Weak);
                continue;
            }
            _ => {}
        }

        add_key(&mut keys, sorted_join(&parts), Grade::Strong);

        // "Last, First Middle" and "First Middle Last" put the surname at
        // opposite ends, so read the comma rather than guessing by position.
        let (given, surname) = if src.contains(',') { (parts[1], parts[0]) } else { (parts[0], parts[parts.len() - 1]) };

        // Middle names and initials should not block a match.
        add_key(&mut keys, sorted_join(&[given, surname]), Grade::Strong);

        // Nicknames and handles: Nick/Nicholas, Tom/Thomas, "jhollister".
        let initial = &given[..1];
        add_key(&mut keys, format!("{initial}{surname}"), Grade::Weak);
        add_key(&mut keys, format!("{surname}{initial}"), Grade::Weak);
    }
    keys
}

pub struct Person {
    pub name: String,
    pub keys: Keys,
    pub matched: bool,
}

/// Roster ids are positions in the returned list.
pub fn build_roster(names: &[String]) -> Vec<Person> {
    let mut roster: Vec<Person> =
        names.iter().map(|name| Person { name: name.clone(), keys: name_keys(name, ""), matched: false }).collect();

    // A weak key two people share ("jsmith" for John and Jane Smith) would send
    // one person's commits to the other. Drop it and let an alias settle it.
    let mut owners: HashMap<String, usize> = HashMap::new();
    for person in &roster {
        for (key, grade) in &person.keys {
            if *grade == Grade::Weak {
                *owners.entry(key.clone()).or_default() += 1;
            }
        }
    }
    for (key, count) in owners {
        if count >= 2 {
            for person in &mut roster {
                person.keys.remove(&key);
            }
        }
    }
    roster
}

/// Strong keys are tried across the whole roster before any weak key, so a
/// nickname guess never outranks a real name match.
pub fn find_match(keys: &Keys, roster: &[Person], grade: Grade) -> Option<usize> {
    roster.iter().position(|person| {
        keys.iter().any(|(key, g)| *g == grade && person.keys.get(key) == Some(&grade))
    })
}

pub fn alias_target(name: &str, email: &str, roster: &[Person], aliases: &HashMap<String, String>) -> Option<usize> {
    for candidate in [name, email] {
        if candidate.is_empty() {
            continue;
        }
        if let Some(target) = aliases.get(&candidate.to_lowercase()) {
            if let Some(index) = roster.iter().position(|p| p.name.eq_ignore_ascii_case(target)) {
                return Some(index);
            }
        }
    }
    None
}

/// One distinct name/email pair seen in the events, with how often.
pub struct Identity {
    pub name: String,
    pub email: String,
    pub keys: Keys,
    pub count: usize,
    pub types: HashMap<&'static str, usize>,
}

impl Identity {
    pub fn key(&self) -> String {
        identity_key(&self.name, &self.email)
    }
}

pub fn identity_key(name: &str, email: &str) -> String {
    format!("{name}|{email}")
}

pub struct WeakMatch {
    pub name: String,
    pub email: String,
    pub person: String,
    pub count: usize,
}

/// identity key -> roster id, or -1 for nobody.
pub fn resolve(
    identities: &[Identity],
    roster: &mut [Person],
    aliases: &HashMap<String, String>,
) -> (HashMap<String, i64>, Vec<WeakMatch>) {
    let mut map = HashMap::new();
    let mut pending = vec![];

    for identity in identities {
        let hit = alias_target(&identity.name, &identity.email, roster, aliases)
            .or_else(|| find_match(&identity.keys, roster, Grade::Strong));
        match hit {
            Some(index) => {
                roster[index].matched = true;
                map.insert(identity.key(), index as i64);
            }
            None => pending.push(identity), // give every strong match its chance first
        }
    }

    let mut weak = vec![];
    for identity in pending {
        match find_match(&identity.keys, roster, Grade::Weak) {
            Some(index) => {
                roster[index].matched = true;
                weak.push(WeakMatch {
                    name: identity.name.clone(),
                    email: identity.email.clone(),
                    person: roster[index].name.clone(),
                    count: identity.count,
                });
                map.insert(identity.key(), index as i64);
            }
            None => {
                map.insert(identity.key(), -1);
            }
        }
    }
    (map, weak)
}

/// With no roster configured, chart whoever is most active. One human often
/// shows up as several identities, so cluster on shared keys first.
pub fn discover(identities: &[Identity], max: usize) -> Vec<String> {
    struct Cluster {
        keys: HashSet<String>,
        names: Vec<String>,
        count: usize,
    }

    let mut sorted: Vec<&Identity> = identities.iter().collect();
    sorted.sort_by(|a, b| b.count.cmp(&a.count));

    let mut clusters: Vec<Cluster> = vec![];
    for identity in sorted {
        let index = match clusters.iter().position(|c| identity.keys.keys().any(|k| c.keys.contains(k))) {
            Some(index) => index,
            None => {
                clusters.push(Cluster { keys: HashSet::new(), names: vec![], count: 0 });
                clusters.len() - 1
            }
        };
        let cluster = &mut clusters[index];
        cluster.keys.extend(identity.keys.keys().cloned());
        cluster.names.push(if identity.name.is_empty() { identity.email.clone() } else { identity.name.clone() });
        cluster.count += identity.count;
    }

    clusters.sort_by(|a, b| b.count.cmp(&a.count));
    // Label each cluster with a real display name where one exists, rather
    // than whichever git handle happened to come first.
    clusters
        .iter()
        .take(max)
        .map(|c| c.names.iter().find(|n| n.chars().any(char::is_whitespace)).unwrap_or(&c.names[0]).clone())
        .collect()
}

/// Resolves identities that only turn up later (the longer stats window), by
/// the same rules: known identity, alias, whole name, then nickname guess.
pub struct Resolver<'a> {
    pub known: &'a HashMap<String, i64>,
    pub roster: &'a [Person],
    pub aliases: &'a HashMap<String, String>,
    cache: HashMap<String, i64>,
}

impl<'a> Resolver<'a> {
    pub fn new(known: &'a HashMap<String, i64>, roster: &'a [Person], aliases: &'a HashMap<String, String>) -> Self {
        Self { known, roster, aliases, cache: HashMap::new() }
    }

    pub fn find(&mut self, name: &str, email: &str) -> i64 {
        let key = identity_key(name, email);
        if let Some(id) = self.known.get(&key).or_else(|| self.cache.get(&key)) {
            return *id;
        }
        let keys = name_keys(name, email);
        let id = alias_target(name, email, self.roster, self.aliases)
            .or_else(|| find_match(&keys, self.roster, Grade::Strong))
            .or_else(|| find_match(&keys, self.roster, Grade::Weak))
            .map_or(-1, |i| i as i64);
        self.cache.insert(key, id);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formal_and_git_names_meet() {
        let roster = build_roster(&["Alvarez-CONTRACTOR, Robin".into(), "Ramirez, Nicholas".into()]);
        let strong = name_keys("Robin Alvarez", "robin.alvarez@x.com");
        assert_eq!(find_match(&strong, &roster, Grade::Strong), Some(0));
        let nick = name_keys("Nick Ramirez", "");
        assert_eq!(find_match(&nick, &roster, Grade::Strong), None);
        assert_eq!(find_match(&nick, &roster, Grade::Weak), Some(1));
    }

    #[test]
    fn shared_weak_keys_are_dropped() {
        let roster = build_roster(&["Emerson, Morgan".into(), "Emerson, Mia".into()]);
        assert_eq!(find_match(&name_keys("memerson", ""), &roster, Grade::Weak), None);
    }
}
