//! `pastor profile render`: what a permission profile becomes for one kind
//! of agent, as the flags, env and settings a task under it gets. Claude and
//! agy are rendered; the other kinds are refused until they are. A tool that
//! drives pastor names a profile and writes this itself, so a permission rule
//! lives in one place.
//!
//! Nothing here translates a pattern on its own. The flags and env are what
//! `Agents::launch` gives a task settled under the profile (`Profiles::apply`
//! then `launch`) less what the same agent gets with no profile, and agy's
//! settings are what the agy provisioner writes into an empty file
//! (`provision::agy::merge`), so a render and a launch cannot disagree.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::cli::CliError;
use crate::config::agy::Policy;
use crate::config::profile::{Profile, UNRESTRICTED};
use crate::config::{AgentChoice, Defaults, Layer, PastorConfig};
use crate::task::{AgentSource, DispatchSpec};

/// The kinds of agent a profile renders for.
pub const KINDS: [&str; 2] = ["claude", "agy"];

/// Every kind `--kind` takes: the ones rendered, then the ones refused with
/// `RENDER_KIND_UNSUPPORTED`, so the refusal can say why.
pub const ALL_KINDS: [&str; 4] = ["claude", "agy", "opencode", "codex"];

/// The kinds pastor runs that a profile does not render for yet, in the order
/// they are taken up.
const LATER: [&str; 2] = ["opencode", "codex"];

/// The code of a kind that is not rendered yet.
pub const RENDER_KIND_UNSUPPORTED: &str = "render_kind_unsupported";

/// A pattern the kind has nothing for, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unsupported {
    pub pattern: String,
    pub why: String,
}

/// What a profile becomes for a kind, as `--json` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Render {
    pub profile: String,
    pub kind: String,
    /// Args the agent starts with, after its own.
    pub flags: Vec<String>,
    /// Env the agent starts with, beyond its own.
    pub env: BTreeMap<String, String>,
    /// The profile in the agent's settings file, in the file's own shape;
    /// `null` for a kind that takes it on the command line or in env.
    pub settings: serde_json::Value,
    /// Files to write beside the settings. None yet.
    pub files: Vec<String>,
    /// Patterns of the profile that do not reach the agent.
    pub unsupported: Vec<Unsupported>,
}

fn refuse(code: &str, message: impl std::fmt::Display) -> anyhow::Error {
    CliError::err(code, message.to_string())
}

/// A spec that names `agent` and nothing else: the `[agents]` entry of that
/// name, else the kind's default.
pub(crate) fn base_spec(agent: &str) -> DispatchSpec {
    serde_json::from_value(serde_json::json!({"agent": agent})).expect("a spec of an agent")
}

/// The machine a render is settled for (`--machine`): its name and its own
/// profile, if it has one.
#[derive(Debug, Clone, Copy)]
pub struct OnMachine<'a> {
    pub name: &'a str,
    pub own: Option<&'a str>,
}

/// `profile` for an agent of `kind` under `config`, as for a task on
/// `machine` (`None`: no machine, so the rules that look at one do not
/// apply). `unrestricted` needs the machine's own profile to be it, and an
/// agy task needs the machine's own profile to deny what the profile does:
/// the refusals `Daemon::settle` makes for a task.
pub fn render(
    config: &PastorConfig,
    profile: &Profile,
    kind: &str,
    machine: Option<OnMachine<'_>>,
) -> anyhow::Result<Render> {
    if LATER.contains(&kind) {
        return Err(refuse(
            RENDER_KIND_UNSUPPORTED,
            format!(
                "profile render does not support {kind} yet; it renders {}",
                KINDS.join(" and ")
            ),
        ));
    }
    let agents = &config.agents;
    // The profile settled into a spec, as a task's is.
    let mut pick = Defaults::default().resolve_agent(&AgentChoice::default(), None);
    pick.profile = Some((profile.name.clone(), Layer::Ask));
    let mut spec = base_spec(kind);
    config
        .profiles
        .apply(
            &pick,
            machine.map_or(Some(UNRESTRICTED), |m| m.own),
            &mut spec,
        )
        .map_err(|e| refuse(e.code, e.message))?;
    spec.agent_source = Some(Box::new(AgentSource {
        profile: Some(profile.name.clone()),
        ..Default::default()
    }));
    let own_permissions = agents.own_permissions(kind);
    // agy's rules are per machine, settled from the machine's own profile.
    if agents.agy_profile(&spec)
        && let Some(OnMachine {
            name,
            own: Some(own),
        }) = machine
    {
        let own_deny = config
            .profiles
            .resolve(own)
            .map_err(|e| refuse(e.code, e.message))?
            .deny;
        if let Some(e) = crate::config::agy::profile_mismatch(
            &spec.deny,
            &profile.name,
            own,
            &own_deny,
            Some(name),
        ) {
            return Err(refuse(e.code, e.message));
        }
    }

    let mut unsupported: Vec<Unsupported> = Vec::new();
    let patterns: Vec<String> = spec.allow.iter().chain(&spec.deny).cloned().collect();
    let mut note = |patterns: &[String], why: &str| {
        for p in patterns {
            if !unsupported.iter().any(|u| &u.pattern == p) {
                unsupported.push(Unsupported {
                    pattern: p.clone(),
                    why: why.into(),
                });
            }
        }
    };
    let agy = agents.agy_profile(&spec);
    let mut policy = None;
    if agy {
        // Its rules are settings, not flags. A deny it has no rule for is
        // refused by the launch below (`agy_deny_unsupported`), as for a
        // task; an allow it has no rule for, or would state more widely, is
        // left out and listed.
        let policy_of = Policy {
            allow: spec.allow.clone(),
            deny: spec.deny.clone(),
            options: profile.agy.clone(),
        };
        let allow_no: Vec<String> = spec
            .allow
            .iter()
            .filter(|p| crate::config::agy::allow_rule(p).is_none())
            .cloned()
            .collect();
        let (no_rule, wider): (Vec<String>, Vec<String>) = allow_no
            .into_iter()
            .partition(|p| crate::config::agy::required_rule(p).is_none());
        note(&no_rule, "agy has no rule for it");
        note(
            &wider,
            "agy would state this allow more widely than the profile does, so it is left out",
        );
        policy = Some(policy_of);
    } else if own_permissions {
        note(
            &patterns,
            &format!("{kind}'s permissions are in its own settings on the machine"),
        );
    }

    let mut bare = base_spec(kind);
    bare.agent_args = spec.agent_args.clone();
    let launched = agents
        .launch(&spec)
        .map_err(|e| refuse(e.code, e.message))?;
    let plain = agents
        .launch(&bare)
        .map_err(|e| refuse(e.code, e.message))?;
    // After the agent's own args, which `base_spec` has none of.
    let flags = launched.args[spec.agent_args.len().min(launched.args.len())..].to_vec();
    let env: BTreeMap<String, String> = launched
        .env
        .iter()
        .filter(|(k, v)| plain.env.get(*k) != Some(*v))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let settings = if let Some(policy) = policy {
        match crate::provision::agy::merge(None, &policy).map_err(|e| refuse("render_failed", e))? {
            crate::provision::agy::Merge::Write { content, .. } => {
                serde_json::from_str(&content).map_err(|e| refuse("render_failed", e))?
            }
            crate::provision::agy::Merge::Unchanged => serde_json::Value::Null,
        }
    } else if kind == "claude" && !own_permissions {
        // With the tool every Claude task is allowed, as its flags have it.
        let mut allow = spec.allow.clone();
        if !allow.iter().any(|p| p == crate::config::TASK_DONE_ALLOW) {
            allow.push(crate::config::TASK_DONE_ALLOW.into());
        }
        serde_json::json!({"permissions": {
            "allow": allow,
            "deny": spec.deny,
            "defaultMode": "dontAsk",
        }})
    } else {
        serde_json::Value::Null
    };
    Ok(Render {
        profile: profile.name.clone(),
        kind: kind.to_string(),
        flags,
        env,
        settings,
        files: vec![],
        unsupported,
    })
}

impl Render {
    /// The render as lines for a person.
    pub fn text(&self) -> String {
        let mut out = format!("profile: {}\nkind: {}\n", self.profile, self.kind);
        let flags = if self.flags.is_empty() {
            "-".to_string()
        } else {
            self.flags
                .iter()
                .map(|f| crate::herdr::transport::shell_quote(f))
                .collect::<Vec<_>>()
                .join(" ")
        };
        out.push_str(&format!("flags: {flags}\n"));
        if self.env.is_empty() {
            out.push_str("env: -\n");
        } else {
            out.push_str("env:\n");
            for (k, v) in &self.env {
                out.push_str(&format!("  {k}={}\n", crate::cli::printable(v)));
            }
        }
        if self.settings.is_null() {
            out.push_str("settings: -\n");
        } else {
            out.push_str("settings:\n");
            for line in serde_json::to_string_pretty(&self.settings)
                .unwrap_or_default()
                .lines()
            {
                out.push_str(&format!("  {line}\n"));
            }
        }
        if self.unsupported.is_empty() {
            out.push_str("unsupported: -\n");
        } else {
            out.push_str("unsupported:\n");
            for u in &self.unsupported {
                out.push_str(&format!(
                    "  {}: {}\n",
                    crate::cli::printable(&u.pattern),
                    u.why
                ));
            }
        }
        out
    }
}

#[cfg(all(test, feature = "fake-herdr"))]
mod tests {
    use super::*;
    use crate::herdr::fake::FakeHerdr;

    fn config(text: &str) -> PastorConfig {
        PastorConfig::parse(std::path::Path::new("pastor.toml"), text).unwrap()
    }

    fn render_of(config: &PastorConfig, name: &str, kind: &str) -> Render {
        let profile = config.profiles.resolve(name).unwrap();
        render(config, &profile, kind, None).unwrap()
    }

    /// A profile agy can carry: it has no rule for `Grep` or `Glob` (allows,
    /// so listed) and for an exact `Bash(make)` (left out, so listed).
    const AGY_CI: &str =
        "[profiles.ci]\nextends = \"review\"\nallow = [\"mcp__board__post\", \"Bash(make)\"]\n";

    const CI: &str = "[profiles.ci]\nextends = \"review\"\nallow = [\"mcp__board__post\", \"Bash(make)\"]\n\
                      deny = [\"Glob\", \"WebSearch\"]\n";

    /// What a task under the profile is launched with, settled the way a
    /// head does: `Profiles::apply`, then `Agents::launch`.
    fn launched(config: &PastorConfig, name: &str, kind: &str) -> crate::config::Launch {
        let mut pick = Defaults::default().resolve_agent(&AgentChoice::default(), None);
        pick.profile = Some((name.into(), Layer::Ask));
        let mut spec = base_spec(kind);
        config
            .profiles
            .apply(&pick, Some(UNRESTRICTED), &mut spec)
            .unwrap();
        spec.agent_source = Some(Box::new(AgentSource {
            profile: Some(name.into()),
            ..Default::default()
        }));
        config.agents.launch(&spec).unwrap()
    }

    /// Claude: the flags are the task's, and the settings carry the same
    /// lists in the file's own shape; nothing is left out.
    #[test]
    fn claude_gets_the_flags_and_the_settings_of_a_launch() {
        let c = config(CI);
        let r = render_of(&c, "ci", "claude");
        let l = launched(&c, "ci", "claude");
        let plain = c.agents.launch(&base_spec("claude")).unwrap();
        assert_eq!(r.flags, l.args);
        assert_eq!(l.env.len(), plain.env.len());
        assert!(r.env.is_empty());
        assert_eq!(&r.flags[..2], ["--permission-mode", "dontAsk"]);
        assert!(
            r.flags
                .windows(2)
                .any(|w| w == ["--allowedTools", "mcp__board__post"])
        );
        assert!(
            r.flags
                .windows(2)
                .any(|w| w == ["--disallowedTools", "Glob"])
        );
        let permissions = &r.settings["permissions"];
        assert_eq!(permissions["defaultMode"], "dontAsk");
        assert!(
            permissions["allow"]
                .as_array()
                .unwrap()
                .contains(&"Bash(make)".into())
        );
        assert!(
            permissions["deny"]
                .as_array()
                .unwrap()
                .contains(&"Glob".into())
        );
        assert!(r.unsupported.is_empty() && r.files.is_empty());
        // A deny wins: the allow review has for Glob is gone.
        assert!(
            !permissions["allow"]
                .as_array()
                .unwrap()
                .contains(&"Glob".into())
        );
    }

    /// agy: the flags of a launch, and settings that are what the agy
    /// provisioner writes into an empty file. A pattern it has no rule for is
    /// listed, an inexact allow too.
    #[tokio::test]
    async fn agy_settings_are_what_the_provisioner_writes() {
        let c = config(&format!(
            "{AGY_CI}[profiles.ci.agy]\nqueued_messages = \"send-immediately\"\n"
        ));
        let r = render_of(&c, "ci", "agy");
        assert_eq!(r.flags, launched(&c, "ci", "agy").args);
        let profile = c.profiles.resolve("ci").unwrap();
        let fake = FakeHerdr::new();
        fake.set_agy_settings(Some(crate::provision::agy::AgySettings {
            content: None,
            link: false,
        }));
        crate::provision::agy::reconcile(&fake, Some(&profile.agy_policy()), "m")
            .await
            .unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&fake.agy_settings_now().unwrap().content.unwrap()).unwrap();
        assert_eq!(r.settings, written);
        assert_eq!(r.settings["queuedMessages"], "send-immediately");
        let unsupported: Vec<(&str, bool)> = r
            .unsupported
            .iter()
            .map(|u| (u.pattern.as_str(), u.why.contains("more widely")))
            .collect();
        assert!(unsupported.contains(&("Glob", false)), "{unsupported:?}");
        assert!(unsupported.contains(&("Grep", false)), "{unsupported:?}");
        assert!(
            unsupported.contains(&("Bash(make)", true)),
            "{unsupported:?}"
        );
        // What it can carry is not listed.
        assert!(!unsupported.iter().any(|(p, _)| *p == "mcp__board__post"));
    }

    /// A deny agy has no rule for is refused as a task under the profile is
    /// (`agy_deny_unsupported`), not rendered without it; the same message as
    /// the launch's.
    #[test]
    fn an_agy_deny_with_no_rule_is_refused_as_a_launch_refuses_it() {
        let c = config(CI);
        let profile = c.profiles.resolve("ci").unwrap();
        let err = render(&c, &profile, "agy", None).unwrap_err();
        let err = err.downcast_ref::<CliError>().unwrap();
        assert_eq!(err.code, crate::config::agy::AGY_DENY_UNSUPPORTED);
        // The launch of the settled task says the same.
        let mut pick = Defaults::default().resolve_agent(&AgentChoice::default(), None);
        pick.profile = Some(("ci".into(), Layer::Ask));
        let mut spec = base_spec("agy");
        c.profiles
            .apply(&pick, Some(UNRESTRICTED), &mut spec)
            .unwrap();
        spec.agent_source = Some(Box::new(AgentSource {
            profile: Some("ci".into()),
            ..Default::default()
        }));
        let refused = c.agents.launch(&spec).unwrap_err();
        assert_eq!(refused.code, err.code);
        assert_eq!(refused.message, err.message);
    }

    /// agy's rules are per machine: with `--machine`, a profile that denies
    /// what the machine's own does not is refused as `Daemon::settle` refuses
    /// the task (`agy_profile_mismatch`), and one the machine's own covers, or
    /// a machine with no profile of its own, is not.
    #[test]
    fn an_agy_render_for_a_machine_needs_the_machines_own_profile_to_cover_it() {
        let c = config("");
        let review = c.profiles.resolve("review").unwrap();
        let on = |own| Some(OnMachine { name: "pi-3", own });
        let err = render(&c, &review, "agy", on(Some("develop"))).unwrap_err();
        let err = err.downcast_ref::<CliError>().unwrap();
        assert_eq!(err.code, crate::config::agy::AGY_PROFILE_MISMATCH);
        assert!(
            err.message.contains("machine pi-3 (develop)"),
            "{}",
            err.message
        );
        assert!(render(&c, &review, "agy", on(Some("review"))).is_ok());
        assert!(render(&c, &review, "agy", on(None)).is_ok());
        assert!(render(&c, &review, "agy", None).is_ok());
        // Claude's rules are per task: no such refusal.
        assert!(render(&c, &review, "claude", on(Some("develop"))).is_ok());
    }

    /// The kinds that come later are refused by name, with the ones that are
    /// rendered, not rendered half.
    #[test]
    fn opencode_and_codex_are_refused_for_now() {
        let c = config(CI);
        let profile = c.profiles.resolve("ci").unwrap();
        for kind in ["opencode", "codex"] {
            let err = render(&c, &profile, kind, None).unwrap_err();
            let err = err.downcast_ref::<CliError>().unwrap();
            assert_eq!(err.code, RENDER_KIND_UNSUPPORTED);
            assert!(
                err.message
                    .contains(&format!("does not support {kind} yet"))
                    && err.message.contains("claude and agy"),
                "{}",
                err.message
            );
        }
    }

    /// An agent whose permissions are its own carries nothing, and every
    /// pattern says so.
    #[test]
    fn an_agent_with_its_own_permissions_carries_nothing() {
        let c = config(&format!("{CI}[agents.claude]\nown_permissions = true\n"));
        let r = render_of(&c, "ci", "claude");
        assert!(r.flags.is_empty() && r.settings.is_null());
        assert!(!r.unsupported.is_empty());
        assert!(
            r.unsupported.iter().all(|u| u.why.contains("own settings")),
            "{:?}",
            r.unsupported
        );
    }

    /// `unrestricted` renders where the machine's own profile is
    /// `unrestricted`, and without a machine; anywhere else it is refused as
    /// it is for a task.
    #[test]
    fn unrestricted_follows_the_machines_own_profile() {
        let c = config("");
        let p = c.profiles.resolve("unrestricted").unwrap();
        let on = |own| Some(OnMachine { name: "m", own });
        let err = render(&c, &p, "claude", on(Some("develop"))).unwrap_err();
        assert_eq!(
            err.downcast_ref::<CliError>().unwrap().code,
            "profile_not_allowed"
        );
        assert!(render(&c, &p, "claude", on(Some("unrestricted"))).is_ok());
        assert!(render(&c, &p, "claude", on(None)).is_err());
        assert!(render(&c, &p, "claude", None).is_ok());
    }

    #[test]
    fn the_text_names_everything() {
        let c = config(AGY_CI);
        let text = render_of(&c, "ci", "agy").text();
        for want in [
            "profile: ci\n",
            "kind: agy\n",
            "flags: --dangerously-skip-permissions",
            "settings:\n",
            "unsupported:\n  Glob: agy has no rule",
        ] {
            assert!(text.contains(want), "{want}: {text}");
        }
    }
}
