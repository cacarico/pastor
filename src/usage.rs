//! The tokens a Claude task used, read from its session file on the machine
//! it ran on once it ends (`Connector::claude_usage`).
//!
//! Claude writes each session to `<config>/projects/<dir>/<session>.jsonl`,
//! `<config>` being `CLAUDE_CONFIG_DIR` or `~/.claude`, and the subagents it
//! starts to `<dir>/<session>/subagents/*.jsonl`. Every API reply is an
//! assistant line with the message's `model`, `id` and `usage`; a reply with
//! several content blocks is written as several lines with the same id, so
//! lines are counted by id, the last one read winning. The files can be
//! large, so the sums are made on the machine by `awk` and only one line
//! comes back.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::herdr::transport::shell_quote;

/// What a task's Claude session used, as totals over its API calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskUsage {
    /// The model of the session's last reply; `None` when no reply named one.
    /// Subagents' models are not counted here, their tokens are.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub api_calls: u64,
    pub input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
    /// When pastor read the session file.
    pub read_at: DateTime<Utc>,
}

/// Sums every assistant line of the files it is given, one reply per
/// message id, into `usage <calls> <input> <cache write> <cache read>
/// <output> <model>`, `-` for no model, or `none` for no reply. Only the
/// keys after the line's last `"usage":{` count, so a tool input that
/// happens to hold the same keys does not; `<synthetic>` replies are
/// Claude's own error notes, not API calls. No backslash, so no login shell
/// reads it as an escape (`posix_command`).
const AWK_SUMS: &str = r#"function num(s, key,  k) { k = q key q ":"; if (match(s, k "[0-9]+")) return substr(s, RSTART + length(k), RLENGTH - length(k)) + 0; return 0 }
function str(s, key,  k) { k = q key q ":" q; if (match(s, k "[^" q "]*")) return substr(s, RSTART + length(k), RLENGTH - length(k)); return "" }
BEGIN { q = sprintf("%c", 34) }
index($0, q "role" q ":" q "assistant" q) && index($0, q "usage" q ":{") {
  m = str($0, "model")
  if (m == "<synthetic>") next
  id = str($0, "id")
  if (id == "") id = str($0, "requestId")
  if (id == "") id = FILENAME ":" FNR
  u = $0
  while ((i = index(u, q "usage" q ":{")) > 0) u = substr(u, i + 9)
  inp[id] = num(u, "input_tokens")
  cw[id] = num(u, "cache_creation_input_tokens")
  cr[id] = num(u, "cache_read_input_tokens")
  out[id] = num(u, "output_tokens")
  if (m != "" && !index(FILENAME, "/subagents/")) model = m
}
END {
  for (id in inp) { n++; a += inp[id]; b += cw[id]; c += cr[id]; d += out[id] }
  if (n == 0) { print "none"; exit }
  if (model == "") model = "-"
  printf "usage %.0f %.0f %.0f %.0f %.0f %s", n, a, b, c, d, model
  print ""
}"#;

/// Where Claude keeps its files for an agent whose env sets
/// `CLAUDE_CONFIG_DIR` to `config_dir` (as written in `pastor.toml`, `~`
/// and all), or `~/.claude` without it, as a shell word the machine's shell
/// expands.
fn config_dir_word(config_dir: Option<&str>) -> String {
    match config_dir {
        None => "\"$HOME\"/.claude".into(),
        Some("~") => "\"$HOME\"".into(),
        Some(dir) => match dir.strip_prefix("~/") {
            Some(rest) => format!("\"$HOME\"/{}", shell_quote(rest)),
            None => shell_quote(dir),
        },
    }
}

/// The shell command that sums session `session`'s usage on the machine,
/// its subagents' too, answered on stdout as `AWK_SUMS` says. `none` when
/// there is no such session file.
pub fn usage_command(config_dir: Option<&str>, session: &str) -> String {
    let dir = config_dir_word(config_dir);
    let s = shell_quote(session);
    format!(
        "d={dir}; set --; for f in \"$d\"/projects/*/{s}.jsonl \"$d\"/projects/*/{s}/subagents/*.jsonl; do if test -f \"$f\"; then set -- \"$@\" \"$f\"; fi; done; if test $# -eq 0; then echo none; else awk {} \"$@\"; fi",
        shell_quote(AWK_SUMS)
    )
}

/// Reads the answer to `usage_command`: the last line of stdout, since rc
/// files may print before it. `None` for `none` or anything else.
pub fn parse_usage(stdout: &str, read_at: DateTime<Utc>) -> Option<TaskUsage> {
    let last = stdout.trim_end().lines().last()?;
    let words: Vec<&str> = last.split_whitespace().collect();
    let ["usage", calls, input, cw, cr, output, model] = words.as_slice() else {
        return None;
    };
    let n = |s: &str| s.parse::<u64>().ok();
    Some(TaskUsage {
        model: (*model != "-").then(|| model.to_string()),
        api_calls: n(calls)?,
        input: n(input)?,
        cache_write: n(cw)?,
        cache_read: n(cr)?,
        output: n(output)?,
        read_at,
    })
}

/// `CLAUDE_CONFIG_DIR` in the env of agent `agent`, as `pastor.toml` sets it.
pub fn config_dir<'a>(agents: &'a crate::config::Agents, agent: &str) -> Option<&'a str> {
    agents
        .0
        .get(agent)
        .and_then(|d| d.env.get("CLAUDE_CONFIG_DIR"))
        .map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "0d5bd3a4-2f35-4e1c-9f59-7c1c3a7b8e21";

    /// An assistant line with its keys in the order Claude writes them.
    fn reply(id: &str, model: &str, input: u64, cw: u64, cr: u64, out: u64) -> String {
        format!(
            r#"{{"parentUuid":null,"isSidechain":false,"message":{{"model":"{model}","id":"{id}","type":"message","role":"assistant","content":[{{"type":"tool_use","id":"toolu_1","name":"Agent","input":{{"model":"haiku","usage":{{"input_tokens":999999}}}}}}],"stop_reason":null,"usage":{{"input_tokens":{input},"cache_creation_input_tokens":{cw},"cache_read_input_tokens":{cr},"output_tokens":{out},"output_tokens_details":{{"thinking_tokens":1}},"cache_creation":{{"ephemeral_1h_input_tokens":{cw}}},"iterations":[{{"input_tokens":7777,"output_tokens":7777}}]}}}},"requestId":"req_1","type":"assistant","sessionId":"{SESSION}"}}"#
        )
    }

    fn run(home: &std::path::Path, config_dir: Option<&str>) -> std::process::Output {
        std::process::Command::new("sh")
            .args(["-c", &usage_command(config_dir, SESSION)])
            .env("HOME", home)
            .output()
            .unwrap()
    }

    /// Replies are counted once per message id with the last line's counts,
    /// the user's lines and Claude's synthetic notes not at all, and a
    /// subagent's tokens are added in while its model is not the session's.
    #[test]
    fn the_command_sums_a_session_and_its_subagents() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude/projects/-src-app");
        std::fs::create_dir_all(project.join(SESSION).join("subagents")).unwrap();
        let user = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "go"},
            "toolUseResult": {"usage": {"input_tokens": 5000}}
        });
        let synthetic = reply("msg_x", "<synthetic>", 0, 0, 0, 0);
        let lines = [
            user.to_string(),
            reply("msg_a", "claude-opus-4-5", 1, 10, 100, 5),
            reply("msg_a", "claude-opus-4-5", 1, 10, 100, 20),
            synthetic,
            reply("msg_b", "claude-sonnet-4-5", 2, 20, 200, 30),
        ];
        std::fs::write(
            project.join(format!("{SESSION}.jsonl")),
            lines.join("\n") + "\n",
        )
        .unwrap();
        std::fs::write(
            project.join(SESSION).join("subagents/agent-1.jsonl"),
            reply("msg_c", "claude-haiku-4-5", 3, 30, 300, 40) + "\n",
        )
        .unwrap();
        let out = run(home.path(), None);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{out:?}");
        let at = Utc::now();
        assert_eq!(
            parse_usage(&stdout, at),
            Some(TaskUsage {
                model: Some("claude-sonnet-4-5".into()),
                api_calls: 3,
                input: 6,
                cache_write: 60,
                cache_read: 600,
                output: 90,
                read_at: at,
            }),
            "{stdout}"
        );
    }

    /// `CLAUDE_CONFIG_DIR` is where the files are, `~` meaning the machine's
    /// home; no session file answers `none`.
    #[test]
    fn the_command_looks_in_the_agents_config_dir() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join(".claude-personal/projects/p");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join(format!("{SESSION}.jsonl")),
            reply("msg_a", "claude-opus-4-5", 1, 2, 3, 4),
        )
        .unwrap();
        let none = run(home.path(), None);
        assert_eq!(String::from_utf8_lossy(&none.stdout).trim(), "none");
        assert_eq!(
            parse_usage(&String::from_utf8_lossy(&none.stdout), Utc::now()),
            None
        );
        for dir in [
            "~/.claude-personal".to_string(),
            home.path().join(".claude-personal").display().to_string(),
        ] {
            let out = run(home.path(), Some(&dir));
            let u = parse_usage(&String::from_utf8_lossy(&out.stdout), Utc::now())
                .unwrap_or_else(|| panic!("{dir}: {out:?}"));
            assert_eq!((u.api_calls, u.output), (1, 4), "{dir}");
        }
    }

    /// rc-file noise before the answer is skipped; a line that is not an
    /// answer is none.
    #[test]
    fn parse_reads_the_last_line() {
        let at = Utc::now();
        let u = parse_usage("welcome\nusage 2 1 2 3 4 -\n", at).unwrap();
        assert_eq!(u.model, None);
        assert_eq!(u.cache_read, 3);
        assert_eq!(parse_usage("usage 2 1 2\n", at), None);
        assert_eq!(parse_usage("usage x 1 2 3 4 m", at), None);
        assert_eq!(parse_usage("", at), None);
    }
}
