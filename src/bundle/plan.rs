//! What the head renders for one machine: the bundle versions its homes
//! link into, and an item per thing to place, each in the home of the
//! agents it is for. The machine applies the plan (`bundle::apply`).
//!
//! The head renders every var it knows: the flocks' `bundle_vars` and
//! `{machine}`, `{flock}`, `{agent}`, `{kind}`, `{account}`. Paths stay as
//! `~/...`, and the vars that are paths on the machine (`{home}`,
//! `{config_dir}`, `{bundle_dir}`, `{bin}`) are left for the machine, which
//! knows its home.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::manifest::{Manifest, Slot};
use super::slots::BIN;
use super::store;
use crate::config::Paths;
use crate::config::flock::{Flock, MachineConfig};
use crate::harness_manifest::McpPlacement;
use crate::machine_check::{Config, agents_on};

/// The plan's file in a rendered set; the versions sit beside it under
/// `STORE_DIR/<bundle>/<hash>/`.
pub const PLAN_FILE: &str = "plan.json";
pub const STORE_DIR: &str = "store";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub machine: String,
    pub versions: Vec<Version>,
    pub items: Vec<Item>,
    /// What a bundle asked for that no home here has a place for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<Skipped>,
    /// The homes' secret files, as their harness manifests list them, `*`
    /// standing for any characters in a name: the machine refuses to read
    /// or write them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
}

/// A bundle version the items point into.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub bundle: String,
    pub hash: String,
    /// File modes the bundle sets, by path inside it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub modes: BTreeMap<String, u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub bundle: String,
    /// The harness home it is in, as `~/...`; `None` for a file every home
    /// shares (`bin`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<String>,
    /// Where it goes, as `~/...`.
    pub path: String,
    #[serde(flatten)]
    pub what: What,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum What {
    /// A link to `target`, a path in the machine's bundle store
    /// (`<bundle>/<hash>/<src>`).
    Link { target: String },
    /// Pastor's marked block in an instruction file, with this text.
    Block { text: String },
    /// A key of a JSON file, at this path of object keys, set to `value`.
    Key {
        key: Vec<String>,
        value: serde_json::Value,
    },
    /// An entry pastor adds to the list at `key` in a JSON file, such as a
    /// hook group: the user's entries around it are theirs.
    Entry {
        key: Vec<String>,
        value: serde_json::Value,
    },
    /// A whole file pastor writes, outside the user's files: the plugin
    /// that carries Claude's MCP servers, or a workdir set's file. `mode`
    /// is the one it is written with; unset, a new file gets `0644` and a
    /// file there keeps its own.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
    },
}

impl What {
    /// What tells two items at one path apart: their key, and for a list
    /// entry its content too.
    pub fn slot(&self) -> (Option<&[String]>, Option<String>) {
        match self {
            What::Key { key, .. } => (Some(key), None),
            What::Entry { key, value } => (Some(key), Some(value_sha(value))),
            What::Link { .. } | What::Block { .. } | What::Text { .. } => (None, None),
        }
    }
}

/// The SHA-256 of a JSON value as compact JSON, its keys sorted.
pub fn value_sha(value: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(value.to_string().as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The bundle name pastor's own Claude plugin goes by in a plan, the
/// ledger and the rows; no bundle may take it.
pub const MCP_PLUGIN: &str = "pastor-mcp";

/// Where pastor keeps a home's Claude plugin marketplace, one per home.
pub const MARKETPLACES: &str = "~/.local/share/pastor/claude-marketplace";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skipped {
    pub bundle: String,
    pub home: String,
    pub what: String,
    pub why: String,
}

/// A harness home on the machine and the agents that use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Home {
    pub kind: String,
    /// As `~/...`, or as an agent's env gives it.
    pub path: String,
    pub agents: Vec<String>,
}

/// The harness homes of the agents that can land on `m`, one per path and
/// kind, in path order.
pub fn homes_of(flock: &Flock, config: &Config<'_>, m: &MachineConfig) -> Vec<Home> {
    let mut homes: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for a in agents_on(m, flock, config) {
        let Some(harness) = config.harnesses.get(&a.kind) else {
            continue;
        };
        homes
            .entry((harness.paths.home_for(&a.env), a.kind.clone()))
            .or_default()
            .push(a.name);
    }
    homes
        .into_iter()
        .map(|((path, kind), agents)| Home { kind, path, agents })
        .collect()
}

/// Render `m`'s plan from flock.toml, pastor.toml's agents and the newest
/// version kept of each bundle it gets.
pub fn render(
    paths: &Paths,
    flock: &Flock,
    config: &Config<'_>,
    m: &MachineConfig,
) -> Result<Plan, String> {
    let homes = homes_of(flock, config, m);
    let flocks: Vec<&str> = flock
        .flocks_of(m)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    let mut vars: BTreeMap<String, String> = BTreeMap::new();
    for name in &flocks {
        if let Some(f) = flock.entry(name) {
            for (k, v) in &f.bundle_vars {
                vars.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
    vars.insert("machine".into(), m.name.clone());
    let mut plan = Plan {
        machine: m.name.clone(),
        ..Plan::default()
    };
    for home in &homes {
        if let Some(h) = config.harnesses.get(&home.kind) {
            plan.secrets.extend(h.paths.secrets_in(&home.path));
        }
    }
    plan.secrets.sort();
    plan.secrets.dedup();
    for id in flock.bundles_of(m) {
        let versions = store::read_versions(paths, &id).map_err(|e| format!("{e:#}"))?;
        let Some(newest) = versions.first() else {
            return Err(format!(
                "bundle {id} has no version kept; add it with `pastor bundle add`"
            ));
        };
        let dir = paths.bundle_versions_dir(&id).join(&newest.hash);
        let manifest =
            Manifest::load(&dir).map_err(|e| format!("bundle {id} at {}: {e}", newest.hash))?;
        if manifest.target() != super::manifest::Target::Home {
            return Err(format!(
                "bundle {id} goes into tasks' working directories (target = \"workdir\"), not a home: name it in `workdir_bundles`"
            ));
        }
        let named_by = flocks
            .iter()
            .find(|f| flock.entry(f).is_some_and(|e| e.bundles.contains(&id)))
            .or(flocks.first())
            .copied()
            .unwrap_or_default();
        let mut vars = vars.clone();
        vars.insert("flock".into(), named_by.to_string());
        render_bundle(
            &mut plan,
            &manifest,
            &newest.hash,
            &dir,
            &homes,
            config.harnesses,
            &vars,
        );
    }
    for home in &homes {
        if plan
            .items
            .iter()
            .any(|i| i.path == mcp_json(&home.path) && i.bundle != MCP_PLUGIN)
        {
            // The manifest guarantees a plugin harness a settings file.
            let settings = config
                .harnesses
                .get(&home.kind)
                .and_then(|h| h.paths.settings.as_deref())
                .unwrap_or("settings.json");
            plan.items.extend(claude_plugin(&home.path, settings));
        }
    }
    // An instruction file holds one block per bundle, and a JSON file a key
    // per bundle; anything else at one place is two bundles wanting it.
    let mut seen: BTreeMap<(&str, Option<&[String]>), &Item> = BTreeMap::new();
    for item in &plan.items {
        let (key, entry) = item.what.slot();
        if entry.is_some() {
            continue;
        }
        if let Some(other) = seen.insert((&item.path, key), item) {
            let blocks = matches!(
                (&other.what, &item.what),
                (What::Block { .. }, What::Block { .. })
            ) && other.bundle != item.bundle;
            if blocks {
                continue;
            }
            return Err(match key {
                Some(key) => format!(
                    "bundles {} and {} both set {} in {}",
                    other.bundle,
                    item.bundle,
                    key.join("."),
                    item.path
                ),
                None => format!(
                    "bundles {} and {} both place {}",
                    other.bundle, item.bundle, item.path
                ),
            });
        }
    }
    Ok(plan)
}

/// A home's marketplace dir: its path below `~`, `/` as `%`.
fn marketplace(home: &str) -> String {
    let below = home.strip_prefix("~/").unwrap_or(home);
    format!("{MARKETPLACES}/{}", below.replace('/', "%"))
}

fn mcp_json(home: &str) -> String {
    format!("{}/plugins/{MCP_PLUGIN}/.mcp.json", marketplace(home))
}

/// The rest of a home's Claude plugin, once its `.mcp.json` has a server:
/// the marketplace and plugin manifests, then the two settings keys that
/// enable it, last so Claude never meets the plugin half written. Claude
/// reads a directory marketplace in place and needs no install step. The
/// keys go in the harness's settings file, `settings` below the home.
fn claude_plugin(home: &str, settings: &str) -> Vec<Item> {
    let market = marketplace(home);
    let item = |path: String, what: What| Item {
        bundle: MCP_PLUGIN.into(),
        home: Some(home.to_string()),
        path,
        what,
    };
    let text = |v: serde_json::Value| What::Text {
        text: format!("{v:#}\n"),
        mode: None,
    };
    let settings = format!("{home}/{settings}");
    vec![
        item(
            format!("{market}/.claude-plugin/marketplace.json"),
            text(serde_json::json!({
                "name": "pastor",
                "owner": {"name": "pastor"},
                "plugins": [{
                    "name": MCP_PLUGIN,
                    "source": format!("./plugins/{MCP_PLUGIN}"),
                    "description": "MCP servers from pastor's bundles",
                }],
            })),
        ),
        item(
            format!("{market}/plugins/{MCP_PLUGIN}/.claude-plugin/plugin.json"),
            text(serde_json::json!({
                "name": MCP_PLUGIN,
                "version": "1.0.0",
                "description": "MCP servers from pastor's bundles",
            })),
        ),
        item(
            settings.clone(),
            What::Key {
                key: vec!["extraKnownMarketplaces".into(), "pastor".into()],
                value: serde_json::json!({
                    "source": {
                        "source": "directory",
                        "path": format!("{{home}}/{}", market.trim_start_matches("~/")),
                    }
                }),
            },
        ),
        item(
            settings,
            What::Key {
                key: vec!["enabledPlugins".into(), format!("{MCP_PLUGIN}@pastor")],
                value: serde_json::Value::Bool(true),
            },
        ),
    ]
}

fn render_bundle(
    plan: &mut Plan,
    m: &Manifest,
    hash: &str,
    dir: &Path,
    homes: &[Home],
    harnesses: &crate::harness_manifest::Harnesses,
    vars: &BTreeMap<String, String>,
) {
    let id = &m.id;
    let target = |src: &str| format!("{id}/{hash}/{}", src.trim_start_matches("./"));
    let name = |src: &str| {
        src.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(src)
            .to_string()
    };
    let wants = |kinds: &Option<Vec<String>>, kind: &str| {
        kinds.as_ref().is_none_or(|k| k.iter().any(|x| x == kind))
    };
    let mut skip = |home: &Home, what: String, why: String| {
        plan.skipped.push(Skipped {
            bundle: id.clone(),
            home: home.path.clone(),
            what,
            why,
        });
    };
    let mut items = Vec::new();
    for home in homes {
        let harness = harnesses.get(&home.kind).expect("homes have manifests");
        let slots = &harness.paths;
        let mut home_vars = vars.clone();
        let agent = home.agents.first().cloned().unwrap_or_default();
        home_vars.insert("agent".into(), agent.clone());
        home_vars.insert("account".into(), agent);
        home_vars.insert("kind".into(), home.kind.clone());
        for s in &m.skills {
            if !wants(&s.kinds, &home.kind) {
                continue;
            }
            match slots.skills.as_deref() {
                Some(sub) => items.push(Item {
                    bundle: id.clone(),
                    home: Some(home.path.clone()),
                    path: format!("{}/{sub}/{}", home.path, name(&s.src)),
                    what: What::Link {
                        target: target(&s.src),
                    },
                }),
                // Unset kinds mean the kinds with a skills dir.
                None if s.kinds.is_some() => skip(
                    home,
                    format!("skill {}", name(&s.src)),
                    format!("{} has no skills dir", home.kind),
                ),
                None => {}
            }
        }
        for f in m.files.iter().filter(|f| f.slot == Some(Slot::Hooks)) {
            if let Some(sub) = slots.hooks.as_deref() {
                items.push(Item {
                    bundle: id.clone(),
                    home: Some(home.path.clone()),
                    path: format!("{}/{sub}/{}", home.path, name(&f.src)),
                    what: What::Link {
                        target: target(&f.src),
                    },
                });
            }
        }
        if let Some(ins) = &m.instructions
            && wants(&ins.kinds, &home.kind)
        {
            match slots.instructions.as_deref() {
                Some(file) => {
                    let mut text = std::fs::read_to_string(dir.join(&ins.src)).unwrap_or_default();
                    if let Some(per) = &ins.per_machine {
                        let per = per.replace("{machine}", &vars["machine"]);
                        if let Ok(more) = std::fs::read_to_string(dir.join(per)) {
                            text = format!("{}\n\n{}", text.trim_end(), more);
                        }
                    }
                    items.push(Item {
                        bundle: id.clone(),
                        home: Some(home.path.clone()),
                        path: format!("{}/{file}", home.path),
                        what: What::Block {
                            text: fill(&text, &home_vars),
                        },
                    });
                }
                None if ins.kinds.is_some() => skip(
                    home,
                    "instructions".into(),
                    format!("{} has no instruction file", home.kind),
                ),
                None => {}
            }
        }
        if let Some(fragment) = m.settings.get(&home.kind) {
            let text = std::fs::read_to_string(dir.join(&fragment.src)).unwrap_or_default();
            match (slots.settings.as_deref(), serde_json::from_str(&text)) {
                (Some(file), Ok(serde_json::Value::Object(fragment))) => {
                    let path = format!("{}/{file}", home.path);
                    for what in settings_items(&home.kind, fragment) {
                        items.push(Item {
                            bundle: id.clone(),
                            home: Some(home.path.clone()),
                            path: path.clone(),
                            what: fill_what(what, &home_vars),
                        });
                    }
                }
                _ => skip(
                    home,
                    format!("settings.{}", home.kind),
                    format!("pastor does not merge {} settings yet", home.kind),
                ),
            }
        }
        for server in m.mcp.iter().filter(|s| wants(&s.kinds, &home.kind)) {
            let what = format!("mcp server {}", server.name);
            let words =
                |w: &[String]| -> Vec<String> { w.iter().map(|w| fill(w, &home_vars)).collect() };
            let (path, value) = match harness.mcp.as_ref() {
                Some(McpPlacement::Plugin) => {
                    let value = match (&server.command, &server.url) {
                        (Some(c), _) => {
                            let mut v = serde_json::json!({
                                "command": fill(c, &home_vars),
                                "args": words(&server.args),
                            });
                            if !server.env_from.is_empty() {
                                v["env"] = server
                                    .env_from
                                    .iter()
                                    .map(|n| (n.clone(), format!("${{{n}}}").into()))
                                    .collect::<serde_json::Map<_, _>>()
                                    .into();
                            }
                            v
                        }
                        (None, Some(_)) if !server.env_from.is_empty() => {
                            skip(home, what, "a url server takes no env here".into());
                            continue;
                        }
                        (None, Some(u)) => {
                            serde_json::json!({"type": "http", "url": fill(u, &home_vars)})
                        }
                        (None, None) => continue,
                    };
                    (mcp_json(&home.path), value)
                }
                Some(McpPlacement::File(file)) => {
                    if !server.env_from.is_empty() {
                        skip(
                            home,
                            what,
                            format!(
                                "{} takes env values, not names, and pastor does not copy values; give the server its env another way",
                                home.kind
                            ),
                        );
                        continue;
                    }
                    let value = match (&server.command, &server.url) {
                        (Some(c), _) => serde_json::json!({
                            "command": fill(c, &home_vars),
                            "args": words(&server.args),
                        }),
                        (None, Some(u)) => serde_json::json!({"url": fill(u, &home_vars)}),
                        (None, None) => continue,
                    };
                    (file.to_string(), value)
                }
                None => {
                    skip(
                        home,
                        what,
                        format!("pastor does not place {} MCP servers yet", home.kind),
                    );
                    continue;
                }
            };
            items.push(Item {
                bundle: id.clone(),
                home: Some(home.path.clone()),
                path,
                what: What::Key {
                    key: vec!["mcpServers".into(), server.name.clone()],
                    value,
                },
            });
        }
    }
    for f in m.files.iter().filter(|f| f.slot == Some(Slot::Bin)) {
        items.push(Item {
            bundle: id.clone(),
            home: None,
            path: format!("{BIN}/{}", name(&f.src)),
            what: What::Link {
                target: target(&f.src),
            },
        });
    }
    plan.items.extend(items);
    plan.versions.push(Version {
        bundle: id.clone(),
        hash: hash.to_string(),
        modes: m
            .files
            .iter()
            .filter_map(|f| {
                let mode = u32::from_str_radix(f.mode.as_deref()?, 8).ok()?;
                Some((f.src.trim_start_matches("./").to_string(), mode))
            })
            .collect(),
    });
}

/// A settings fragment as items, in the harness's own shape. Claude:
/// each `hooks` group is an entry in its event's list, `env`,
/// `enabledPlugins` and `extraKnownMarketplaces` go key by key, any other
/// key whole. agy: each key whole.
pub(crate) fn settings_items(
    kind: &str,
    fragment: serde_json::Map<String, serde_json::Value>,
) -> Vec<What> {
    let (lists, per_key): (&[&str], &[&str]) = match kind {
        "claude" => (
            &["hooks"],
            &["env", "enabledPlugins", "extraKnownMarketplaces"],
        ),
        _ => (&[], &[]),
    };
    let mut out = Vec::new();
    for (k, v) in fragment {
        match v {
            serde_json::Value::Object(events) if lists.contains(&k.as_str()) => {
                for (event, groups) in events {
                    let key = vec![k.clone(), event.clone()];
                    match groups {
                        serde_json::Value::Array(groups) => {
                            for value in groups {
                                out.push(What::Entry {
                                    key: key.clone(),
                                    value,
                                });
                            }
                        }
                        value => out.push(What::Key { key, value }),
                    }
                }
            }
            serde_json::Value::Object(map) if per_key.contains(&k.as_str()) => {
                for (sub, value) in map {
                    out.push(What::Key {
                        key: vec![k.clone(), sub],
                        value,
                    });
                }
            }
            value => out.push(What::Key {
                key: vec![k],
                value,
            }),
        }
    }
    out
}

/// `what` with the head's vars filled in every string in it.
fn fill_what(what: What, vars: &BTreeMap<String, String>) -> What {
    match what {
        What::Key { key, value } => What::Key {
            key,
            value: fill_value(value, vars),
        },
        What::Entry { key, value } => What::Entry {
            key,
            value: fill_value(value, vars),
        },
        other => other,
    }
}

/// `value` with `vars` filled in each string in it, keys left as they are.
pub fn fill_value(value: serde_json::Value, vars: &BTreeMap<String, String>) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(s) => Value::String(fill(&s, vars)),
        Value::Array(a) => Value::Array(a.into_iter().map(|v| fill_value(v, vars)).collect()),
        Value::Object(o) => Value::Object(
            o.into_iter()
                .map(|(k, v)| (k, fill_value(v, vars)))
                .collect(),
        ),
        other => other,
    }
}

/// `text` with each `{name}` that `vars` holds replaced; any other brace
/// is left as it is.
pub fn fill(text: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}').map(|close| (&after[..close], close)) {
            Some((name, close)) if vars.contains_key(name) => {
                out.push_str(&vars[name]);
                rest = &after[close + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::bundle::store::{Source, add};
    use crate::config::PastorConfig;

    pub(crate) const PASTOR_TOML: &str = "[agents.claude-main]\nkind = \"claude\"\n\
         env = { CLAUDE_CONFIG_DIR = \"~/.claude-main\" }\n[agents.claude-base]\nkind = \"claude\"\n\
         env = { CLAUDE_CONFIG_DIR = \"~/.claude-main\" }\n";

    /// A team-rules bundle with a skill, a hook, a bin file, instructions
    /// with a per-machine part, and a codex-only skill.
    pub(crate) fn add_team_rules(paths: &Paths, root: &Path) {
        let dir = root.join("src/team-rules");
        let put = |rel: &str, text: &str| {
            let p = dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        };
        put(
            "pastor-bundle.toml",
            "id = \"team-rules\"\nversion = \"1\"\n[vars]\nticket_url = {}\n\
             [[skill]]\nsrc = \"skills/reviewing-a-pr\"\n\
             [[skill]]\nsrc = \"skills/codex-only\"\nkinds = [\"codex\"]\n\
             [[file]]\nsrc = \"hooks/notify.sh\"\nslot = \"hooks\"\nmode = \"0755\"\n\
             [[file]]\nsrc = \"bin/team-tool\"\nslot = \"bin\"\nmode = \"0755\"\n\
             [instructions]\nsrc = \"instructions/team.md\"\nper_machine = \"instructions/machines/{machine}.md\"\n",
        );
        put("skills/reviewing-a-pr/SKILL.md", "Read the diff.\n");
        put("skills/codex-only/SKILL.md", "Codex only.\n");
        put("hooks/notify.sh", "#!/bin/sh\necho done\n");
        put("bin/team-tool", "#!/bin/sh\necho tool\n");
        put(
            "instructions/team.md",
            "Tickets: {ticket_url}. You are {agent} on {machine} in {flock}; your home is {config_dir}. {not_a_var}\n",
        );
        put(
            "instructions/machines/build-01.md",
            "This one has the GPUs.\n",
        );
        add(paths, &Source::Dir(dir)).unwrap();
    }

    /// A base bundle with instructions and nothing else.
    pub(crate) fn add_base(paths: &Paths, root: &Path) {
        let dir = root.join("src/base");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pastor-bundle.toml"),
            "id = \"base\"\nversion = \"1\"\n[instructions]\nsrc = \"base.md\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("base.md"), "Run the tests.\n").unwrap();
        add(paths, &Source::Dir(dir)).unwrap();
    }

    pub(crate) const FLOCK: &str = "[[flock]]\nname = \"ops\"\ndefault = true\nagent = \"claude-main\"\n\
         bundles = [\"team-rules\"]\nbundle_vars = { ticket_url = \"https://tickets.example.com\" }\n\
         machines = { build-01 = 1 }\n[[machine]]\nname = \"build-01\"\nlocal = true\n";

    fn plan(paths: &Paths, flock: &str) -> Result<Plan, String> {
        plan_with(
            paths,
            flock,
            PASTOR_TOML,
            crate::harness_manifest::Harnesses::builtin(),
        )
    }

    fn plan_with(
        paths: &Paths,
        flock: &str,
        pastor_toml: &str,
        harnesses: &crate::harness_manifest::Harnesses,
    ) -> Result<Plan, String> {
        let config = PastorConfig::parse(Path::new("pastor.toml"), pastor_toml).unwrap();
        let flock = Flock::parse(Path::new("flock.toml"), flock).unwrap();
        let parts = Config {
            defaults: &config.defaults,
            agents: &config.agents,
            models: &config.models,
            profiles: &config.profiles,
            harnesses,
            herdr_pin: None,
            head_address: None,
        };
        render(paths, &flock, &parts, flock.get("build-01").unwrap())
    }

    #[test]
    fn a_plan_places_skills_hooks_bin_and_a_block_per_home() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        add_team_rules(&paths, tmp.path());
        let plan = plan(&paths, FLOCK).unwrap();
        let hash = &store::read_versions(&paths, "team-rules").unwrap()[0].hash;
        let placed: Vec<(&str, Option<&str>)> = plan
            .items
            .iter()
            .map(|i| (i.path.as_str(), i.home.as_deref()))
            .collect();
        assert_eq!(
            placed,
            [
                (
                    "~/.claude-main/skills/reviewing-a-pr",
                    Some("~/.claude-main")
                ),
                ("~/.claude-main/hooks/notify.sh", Some("~/.claude-main")),
                ("~/.claude-main/CLAUDE.md", Some("~/.claude-main")),
                ("~/.local/bin/team-tool", None),
            ]
        );
        assert_eq!(
            plan.items[0].what,
            What::Link {
                target: format!("team-rules/{hash}/skills/reviewing-a-pr")
            }
        );
        let What::Block { text } = &plan.items[2].what else {
            panic!("{:?}", plan.items[2])
        };
        assert_eq!(
            text,
            "Tickets: https://tickets.example.com. You are claude-main on build-01 in ops; your home is {config_dir}. {not_a_var}\n\n\
             This one has the GPUs.\n"
        );
        assert_eq!(plan.versions[0].modes["hooks/notify.sh"], 0o755);
        // The codex skill names a kind with no home here: nothing to skip.
        assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
    }

    /// Where a bundle's files go is the harness manifest's: one in the
    /// config dir that replaces the built-in claude moves the skills dir and
    /// drops the hooks one, and one for another harness gets homes of its
    /// own, with no Rust.
    #[test]
    fn a_user_manifest_moves_where_a_bundle_places_things() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        add_team_rules(&paths, tmp.path());
        let dir = paths.harnesses_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("claude.toml"),
            "id = \"claude\"\n[paths]\nhome_env = \"CLAUDE_CONFIG_DIR\"\nhome = \"~/.claude\"\n\
             skills = \"my-skills\"\ninstructions = \"RULES.md\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("tool.toml"),
            "id = \"tool\"\n[paths]\nhome = \"~/.tool\"\nskills = \"abilities\"\n",
        )
        .unwrap();
        let harnesses = crate::harness_manifest::Harnesses::load(&paths).unwrap();
        let pastor = format!(
            "{PASTOR_TOML}[agents.tool-1]\nkind = \"tool\"\n[models.m1]\nkind = \"tool\"\nargs = []\n"
        );
        let flock = FLOCK.replace(
            "agent = \"claude-main\"",
            "agent = \"claude-main\"\nagents = { tool = \"tool-1\" }",
        );
        let plan = plan_with(&paths, &flock, &pastor, &harnesses).unwrap();
        let placed: Vec<&str> = plan.items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(
            placed,
            [
                "~/.claude-main/my-skills/reviewing-a-pr",
                "~/.claude-main/RULES.md",
                "~/.tool/abilities/reviewing-a-pr",
                "~/.local/bin/team-tool",
            ],
            "{plan:?}"
        );
    }

    /// How a bundle's MCP servers are placed is the manifest's: a harness
    /// with the file method gets them in its file, outside its home; one
    /// with the plugin method in a plugin in the home; one with no `[mcp]`
    /// gets a skipped note. A user manifest changes any of them, with no Rust.
    #[test]
    fn a_user_manifest_says_how_mcp_servers_are_placed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let dir = tmp.path().join("src/tools");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("pastor-bundle.toml"),
            "id = \"tools\"\nversion = \"1\"\n\
             [[mcp]]\nname = \"docs\"\ncommand = \"docs-mcp\"\nargs = [\"--home\", \"{config_dir}\"]\n",
        )
        .unwrap();
        add(&paths, &Source::Dir(dir)).unwrap();
        let pastor = format!(
            "{PASTOR_TOML}[agents.tool-1]\nkind = \"tool\"\n[models.m1]\nkind = \"tool\"\nargs = []\n\
             [agents.plug-1]\nkind = \"plug\"\n[models.m2]\nkind = \"plug\"\nargs = []\n\
             [agents.bare-1]\nkind = \"bare\"\n[models.m3]\nkind = \"bare\"\nargs = []\n"
        );
        let flock = FLOCK.replace("[\"team-rules\"]", "[\"tools\"]").replace(
            "agent = \"claude-main\"",
            "agents = { tool = \"tool-1\", plug = \"plug-1\", bare = \"bare-1\" }",
        );
        let manifests = paths.harnesses_dir();
        std::fs::create_dir_all(&manifests).unwrap();
        for (id, mcp) in [
            (
                "tool",
                "[mcp]\nmethod = \"file\"\nfile = \"~/.shared/tool-mcp.json\"\n",
            ),
            ("plug", "[mcp]\nmethod = \"plugin\"\n"),
            ("bare", ""),
        ] {
            std::fs::write(
                manifests.join(format!("{id}.toml")),
                format!(
                    "id = \"{id}\"\n{mcp}[paths]\nhome = \"~/.{id}\"\nsettings = \"conf/s.json\"\n"
                ),
            )
            .unwrap();
        }
        let harnesses = crate::harness_manifest::Harnesses::load(&paths).unwrap();
        let plan = plan_with(&paths, &flock, &pastor, &harnesses).unwrap();
        let at = |path: &str| plan.items.iter().find(|i| i.path == path).map(|i| &i.what);
        let key = |what: Option<&What>| match what {
            Some(What::Key { key, value }) => (key.clone(), value.clone()),
            other => panic!("{other:?}"),
        };
        // The file method: the path the manifest names, outside the home.
        let (k, v) = key(at("~/.shared/tool-mcp.json"));
        assert_eq!(k, ["mcpServers", "docs"]);
        assert_eq!(v["command"], "docs-mcp");
        // The machine fills in `{config_dir}`; the plan leaves it.
        assert_eq!(v["args"][1], "{config_dir}");
        // The plugin method: pastor's plugin in the home, enabled in the
        // settings file the manifest names.
        assert!(
            plan.items
                .iter()
                .any(|i| i.home.as_deref() == Some("~/.plug") && i.path.contains("pastor")),
            "{plan:?}"
        );
        let enabled: Vec<&str> = plan
            .items
            .iter()
            .filter(|i| {
                i.bundle == MCP_PLUGIN
                    && i.home.as_deref() == Some("~/.plug")
                    && matches!(i.what, What::Key { .. })
            })
            .map(|i| i.path.as_str())
            .collect();
        assert_eq!(enabled, ["~/.plug/conf/s.json", "~/.plug/conf/s.json"]);
        // No `[mcp]`: skipped, and said so.
        assert!(
            plan.skipped
                .iter()
                .any(|s| s.home == "~/.bare" && s.why.contains("does not place bare MCP servers")),
            "{:?}",
            plan.skipped
        );
    }

    /// Two bundles with instructions each get their block in the one file,
    /// in flock order; two bundles placing one file is an error.
    #[test]
    fn bundles_share_an_instruction_file_but_not_a_file() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        add_team_rules(&paths, tmp.path());
        add_base(&paths, tmp.path());
        let both = FLOCK.replace("[\"team-rules\"]", "[\"team-rules\", \"base\"]");
        let shared = plan(&paths, &both).unwrap();
        let blocks: Vec<(&str, &str)> = shared
            .items
            .iter()
            .filter(|i| matches!(i.what, What::Block { .. }))
            .map(|i| (i.bundle.as_str(), i.path.as_str()))
            .collect();
        assert_eq!(
            blocks,
            [
                ("team-rules", "~/.claude-main/CLAUDE.md"),
                ("base", "~/.claude-main/CLAUDE.md")
            ]
        );

        let dir = tmp.path().join("src/clash");
        std::fs::create_dir_all(dir.join("hooks")).unwrap();
        std::fs::write(
            dir.join("pastor-bundle.toml"),
            "id = \"clash\"\nversion = \"1\"\n\
             [[file]]\nsrc = \"hooks/notify.sh\"\nslot = \"hooks\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("hooks/notify.sh"), "#!/bin/sh\n").unwrap();
        add(&paths, &Source::Dir(dir)).unwrap();
        let clash = FLOCK.replace("[\"team-rules\"]", "[\"team-rules\", \"clash\"]");
        let err = plan(&paths, &clash).unwrap_err();
        assert_eq!(
            err,
            "bundles team-rules and clash both place ~/.claude-main/hooks/notify.sh"
        );
    }

    #[test]
    fn a_bundle_with_no_version_kept_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::new(tmp.path().join("c"), tmp.path().join("s"));
        let err = plan(&paths, FLOCK).unwrap_err();
        assert!(
            err.contains("bundle team-rules has no version kept"),
            "{err}"
        );
    }

    #[test]
    fn fill_replaces_only_known_vars() {
        let vars = BTreeMap::from([("a".to_string(), "1".to_string())]);
        assert_eq!(
            fill("{a} {b} {\"json\": {a}} {", &vars),
            "1 {b} {\"json\": 1} {"
        );
    }
}
