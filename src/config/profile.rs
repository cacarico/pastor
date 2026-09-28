//! Permission profiles: `[profiles.<name>]` in `pastor.toml`, and the three
//! built in, `review`, `develop` and `unrestricted`. A profile is a named
//! pair of tool pattern lists, the `allow` and `deny` of `[defaults]`, that
//! can extend another. A task names one with `profile` like its model
//! (`Defaults::resolve_agent_on`); `Profiles::apply` adds its lists to the
//! task's, and `Agents::launch_args` starts a Claude agent under it so it
//! never asks. `pastor profile list` and `pastor profile describe` show them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{AgentPick, AgentRefusal, check_profile_name, check_tools};

/// One profile under `[profiles.<name>]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileDef {
    /// One line for `pastor profile list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Another profile, built in or not, whose lists come first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    /// Tool patterns the agent may use without asking.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// Tool patterns the agent must never use; wins over `allow`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
}

/// Where a profile's definition comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSource {
    /// Built into pastor, not named in pastor.toml.
    BuiltIn,
    /// `[profiles.<name>]` in pastor.toml.
    Config,
    /// `[profiles.<name>]` in pastor.toml, in place of the built-in one.
    Overrides,
}

impl std::fmt::Display for ProfileSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ProfileSource::BuiltIn => "built-in",
            ProfileSource::Config => "pastor.toml",
            ProfileSource::Overrides => "pastor.toml (overrides built-in)",
        })
    }
}

/// A profile with `extends` followed: the lists every link adds up to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Profile {
    pub name: String,
    pub source: ProfileSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    /// The profile and each it extends, nearest first.
    pub chain: Vec<String>,
    /// Every link's allow, farthest first, once each, less any denied.
    pub allow: Vec<String>,
    /// Every link's deny, farthest first, once each.
    pub deny: Vec<String>,
}

/// The code of a profile name no table defines and none is built in.
pub const UNKNOWN_PROFILE: &str = "unknown_profile";

/// The profile that denies nothing, which a task may ask for only on a
/// machine whose own profile is this one (`Profiles::apply`).
pub const UNRESTRICTED: &str = "unrestricted";

/// The code of a task that asks for `unrestricted` on a machine whose own
/// profile is not.
pub const PROFILE_NOT_ALLOWED: &str = "profile_not_allowed";

/// The tools that read and search, which every built-in profile allows.
const READ_TOOLS: &[&str] = &["Read", "Glob", "Grep"];

/// The built-in profiles, by name. The patterns are Claude Code's.
pub fn builtin() -> BTreeMap<String, ProfileDef> {
    let list = |xs: &[&[&str]]| -> Vec<String> {
        xs.iter()
            .flat_map(|x| x.iter())
            .map(|s| s.to_string())
            .collect()
    };
    let destructive: &[&str] = &["Bash(rm -rf:*)", "Bash(sudo:*)", "Bash(git push --force:*)"];
    BTreeMap::from([
        (
            "review".to_string(),
            ProfileDef {
                description: Some("read the code and its history; change nothing".into()),
                extends: None,
                allow: list(&[
                    READ_TOOLS,
                    &[
                        "Bash(git status:*)",
                        "Bash(git diff:*)",
                        "Bash(git log:*)",
                        "Bash(git show:*)",
                        "Bash(git blame:*)",
                    ],
                ]),
                deny: list(&[
                    &["Edit", "Write", "NotebookEdit", "Bash(git push:*)"],
                    destructive,
                ]),
            },
        ),
        (
            "develop".to_string(),
            ProfileDef {
                description: Some(
                    "edit files and run commands in the checkout; nothing destructive".into(),
                ),
                extends: None,
                allow: list(&[READ_TOOLS, &["Edit", "Write", "NotebookEdit", "Bash"]]),
                deny: list(&[destructive]),
            },
        ),
        (
            "unrestricted".to_string(),
            ProfileDef {
                description: Some("every tool, nothing denied".into()),
                extends: None,
                allow: list(&[
                    READ_TOOLS,
                    &[
                        "Edit",
                        "Write",
                        "NotebookEdit",
                        "Bash",
                        "WebFetch",
                        "WebSearch",
                    ],
                ]),
                deny: vec![],
            },
        ),
    ])
}

/// `[profiles.<name>]`, by name: only what pastor.toml writes. The built-in
/// ones join in `all` and `resolve`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Profiles(pub BTreeMap<String, ProfileDef>);

impl Profiles {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Every profile, built in and written, sorted by name: a table of
    /// pastor.toml takes the place of a built-in one of the same name.
    pub fn all(&self) -> BTreeMap<String, (ProfileDef, ProfileSource)> {
        let mut out: BTreeMap<_, _> = builtin()
            .into_iter()
            .map(|(n, d)| (n, (d, ProfileSource::BuiltIn)))
            .collect();
        for (name, def) in &self.0 {
            let source = if out.contains_key(name) {
                ProfileSource::Overrides
            } else {
                ProfileSource::Config
            };
            out.insert(name.clone(), (def.clone(), source));
        }
        out
    }

    /// `name` with its `extends` followed, or `unknown_profile`. A chain
    /// that names a profile twice is refused; `validate` catches it at load.
    pub fn resolve(&self, name: &str) -> Result<Profile, AgentRefusal> {
        let all = self.all();
        let unknown = |missing: &str, from: Option<&str>| AgentRefusal {
            code: UNKNOWN_PROFILE,
            message: format!(
                "profile {missing}{} is not built in or in [profiles] in pastor.toml; there are {}",
                from.map(|f| format!(" (extended by {f})"))
                    .unwrap_or_default(),
                all.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
        };
        let (first, source) = all.get(name).ok_or_else(|| unknown(name, None))?;
        let mut chain = vec![name.to_string()];
        let mut links = vec![first];
        while let Some(parent) = &links.last().unwrap().extends {
            if chain.contains(parent) {
                return Err(AgentRefusal {
                    code: "profile_cycle",
                    message: format!(
                        "profile {name} extends itself: {} -> {parent}",
                        chain.join(" -> ")
                    ),
                });
            }
            let from = chain.last().unwrap().clone();
            let (def, _) = all
                .get(parent)
                .ok_or_else(|| unknown(parent, Some(&from)))?;
            chain.push(parent.clone());
            links.push(def);
        }
        let mut allow: Vec<String> = Vec::new();
        let mut deny: Vec<String> = Vec::new();
        for def in links.iter().rev() {
            for p in &def.deny {
                if !deny.contains(p) {
                    deny.push(p.clone());
                }
            }
            for p in &def.allow {
                if !allow.contains(p) {
                    allow.push(p.clone());
                }
            }
        }
        allow.retain(|p| !deny.contains(p));
        Ok(Profile {
            name: name.to_string(),
            source: *source,
            description: first.description.clone(),
            extends: first.extends.clone(),
            chain,
            allow,
            deny,
        })
    }

    /// Settle `pick`'s profile's lists into `spec`: before the task's own,
    /// a deny anywhere dropping the pattern from allow. The caller records
    /// the name in `spec.agent_source`, which `Agents::launch_args` reads.
    /// `own` is the profile the machine the
    /// task lands on names for itself (its `[[machine]]` entry, else its
    /// flock's, else `[defaults]`), or the flock's when no machine is
    /// picked yet.
    ///
    /// Refused when the profile is not defined (`unknown_profile`), and,
    /// the unrestricted rule, when it is `unrestricted` and `own` is not
    /// (`profile_not_allowed`): what runs with nothing denied is the
    /// machine's owner's choice, made in flock.toml or `[defaults]`, not
    /// one a run flag or a job file can make for it.
    pub fn apply(
        &self,
        pick: &AgentPick,
        own: Option<&str>,
        spec: &mut crate::task::DispatchSpec,
    ) -> Result<(), AgentRefusal> {
        let Some((name, _)) = &pick.profile else {
            return Ok(());
        };
        let profile = self.resolve(name)?;
        if name == UNRESTRICTED && own != Some(UNRESTRICTED) {
            return Err(AgentRefusal {
                code: PROFILE_NOT_ALLOWED,
                message: format!(
                    "profile {UNRESTRICTED} runs only where the machine's own profile is {UNRESTRICTED} \
                     (its [[machine]] or [[flock]] entry, or [defaults]); here it is {}",
                    own.unwrap_or("none")
                ),
            });
        }
        let mut deny = profile.deny;
        for p in &spec.deny {
            if !deny.contains(p) {
                deny.push(p.clone());
            }
        }
        let mut allow = profile.allow;
        for p in &spec.allow {
            if !allow.contains(p) {
                allow.push(p.clone());
            }
        }
        allow.retain(|p| !deny.contains(p));
        spec.allow = allow;
        spec.deny = deny;
        Ok(())
    }

    /// The load-time check: names in the job names' alphabet, tool patterns
    /// the agent's command line can take, and every `extends` resolving.
    pub fn validate(&self) -> Result<(), String> {
        for (name, def) in &self.0 {
            check_profile_name(name)?;
            check_tools(&format!("profiles.{name}.allow"), &def.allow)?;
            check_tools(&format!("profiles.{name}.deny"), &def.deny)?;
            if let Some(d) = &def.description
                && d.contains(['\n', '\r'])
            {
                return Err(format!("profiles.{name}.description must be one line"));
            }
        }
        for name in self.0.keys() {
            self.resolve(name)
                .map_err(|e| format!("profiles.{name}: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profiles(text: &str) -> Profiles {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn the_builtins_resolve_and_review_changes_nothing() {
        let none = Profiles::default();
        let names: Vec<_> = none.all().into_keys().collect();
        assert_eq!(names, vec!["develop", "review", "unrestricted"]);
        let review = none.resolve("review").unwrap();
        assert_eq!(review.source, ProfileSource::BuiltIn);
        assert_eq!(review.chain, vec!["review"]);
        for tool in ["Edit", "Write", "NotebookEdit"] {
            assert!(review.deny.iter().any(|p| p == tool), "{tool}");
            assert!(!review.allow.iter().any(|p| p == tool), "{tool}");
        }
        assert!(
            none.resolve("develop")
                .unwrap()
                .allow
                .contains(&"Edit".into())
        );
        assert!(none.resolve("unrestricted").unwrap().deny.is_empty());
        for name in builtin().keys() {
            let p = none.resolve(name).unwrap();
            check_tools(name, &p.allow).unwrap();
            check_tools(name, &p.deny).unwrap();
        }
    }

    /// `extends` puts the parent's lists first, adds the child's, and a
    /// deny anywhere in the chain drops that pattern from allow.
    #[test]
    fn extends_adds_the_lists_up_and_deny_wins() {
        let p = profiles(
            "[ci]\nextends = \"base\"\nallow = [\"Bash(docker:*)\", \"Read\"]\ndeny = [\"WebFetch\"]\n\
             [base]\nextends = \"develop\"\nallow = [\"WebFetch\"]\n",
        );
        let ci = p.resolve("ci").unwrap();
        assert_eq!(ci.source, ProfileSource::Config);
        assert_eq!(ci.chain, vec!["ci", "base", "develop"]);
        assert_eq!(ci.extends.as_deref(), Some("base"));
        let develop = Profiles::default().resolve("develop").unwrap();
        assert_eq!(ci.allow[..develop.allow.len()], develop.allow[..]);
        assert_eq!(ci.allow.last().unwrap(), "Bash(docker:*)");
        assert_eq!(ci.allow.iter().filter(|a| *a == "Read").count(), 1);
        assert!(!ci.allow.contains(&"WebFetch".into()));
        assert!(ci.deny.contains(&"WebFetch".into()));
        assert!(ci.deny.contains(&"Bash(sudo:*)".into()));
    }

    fn pick(profile: Option<&str>) -> AgentPick {
        let mut pick = super::super::Defaults::default()
            .resolve_agent(&super::super::AgentChoice::default(), None);
        pick.profile = profile.map(|p| (p.to_string(), super::super::Layer::Ask));
        pick
    }

    fn spec(allow: &[&str], deny: &[&str]) -> crate::task::DispatchSpec {
        let mut spec: crate::task::DispatchSpec =
            serde_json::from_value(serde_json::json!({"agent": "claude"})).unwrap();
        spec.allow = allow.iter().map(|s| s.to_string()).collect();
        spec.deny = deny.iter().map(|s| s.to_string()).collect();
        spec
    }

    /// The profile's lists come first, the task's own after, and a deny
    /// from either side drops the pattern from allow.
    #[test]
    fn apply_puts_the_profiles_lists_first_and_deny_wins() {
        let p = profiles("[ci]\nallow = [\"Read\", \"Bash(make:*)\"]\ndeny = [\"WebFetch\"]\n");
        let mut s = spec(&["Bash(make:*)", "WebFetch", "Edit"], &["Read"]);
        p.apply(&pick(Some("ci")), None, &mut s).unwrap();
        assert_eq!(s.allow, vec!["Bash(make:*)", "Edit"]);
        assert_eq!(s.deny, vec!["WebFetch", "Read"]);

        let mut s = spec(&["Edit"], &[]);
        p.apply(&pick(None), None, &mut s).unwrap();
        assert_eq!(s.allow, vec!["Edit"]);
        let err = p.apply(&pick(Some("gone")), None, &mut s).unwrap_err();
        assert_eq!(err.code, UNKNOWN_PROFILE);
    }

    /// `unrestricted` runs only where the machine's own profile is
    /// `unrestricted`; any other profile runs anywhere.
    #[test]
    fn unrestricted_needs_a_machine_whose_own_profile_is_unrestricted() {
        let none = Profiles::default();
        for own in [None, Some("develop")] {
            let err = none
                .apply(&pick(Some(UNRESTRICTED)), own, &mut spec(&[], &[]))
                .unwrap_err();
            assert_eq!(err.code, PROFILE_NOT_ALLOWED, "{own:?}");
            assert!(err.message.contains(own.unwrap_or("none")), "{err}");
        }
        let mut s = spec(&[], &[]);
        none.apply(&pick(Some(UNRESTRICTED)), Some(UNRESTRICTED), &mut s)
            .unwrap();
        assert!(s.allow.contains(&"WebFetch".to_string()));
        none.apply(
            &pick(Some("review")),
            Some(UNRESTRICTED),
            &mut spec(&[], &[]),
        )
        .unwrap();
    }

    #[test]
    fn a_table_overrides_the_builtin_of_its_name() {
        let p = profiles("[review]\nallow = [\"Read\"]\n");
        let review = p.resolve("review").unwrap();
        assert_eq!(review.source, ProfileSource::Overrides);
        assert_eq!(review.allow, vec!["Read"]);
        assert!(review.deny.is_empty());
    }

    #[test]
    fn unknown_names_cycles_and_bad_patterns_are_refused() {
        let none = Profiles::default();
        let err = none.resolve("nope").unwrap_err();
        assert_eq!(err.code, UNKNOWN_PROFILE);
        assert!(
            err.message.contains("develop, review, unrestricted"),
            "{err}"
        );

        for (text, says) in [
            ("[a]\nextends = \"nope\"\n", "nope (extended by a)"),
            (
                "[a]\nextends = \"b\"\n[b]\nextends = \"a\"\n",
                "a -> b -> a",
            ),
            ("[a]\nextends = \"a\"\n", "extends itself"),
            ("[A]\n", "profile name"),
            ("[a]\nallow = [\"--yolo\"]\n", "profiles.a.allow"),
            ("[a]\ndeny = [\" \"]\n", "profiles.a.deny"),
            ("[a]\ndescription = \"x\\ny\"\n", "one line"),
        ] {
            let err = profiles(text).validate().unwrap_err();
            assert!(err.contains(says), "{text}: {err}");
        }
        assert!(
            toml::from_str::<Profiles>("[a]\nmode = \"x\"\n")
                .unwrap_err()
                .to_string()
                .contains("mode")
        );
    }
}
