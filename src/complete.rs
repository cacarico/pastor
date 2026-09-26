//! `pastor __complete`: the names a shell offers at TAB. The scripts
//! `pastor completions` prints are static, so they cannot know the jobs,
//! flocks, machines, tasks or connectors; the hook they end with asks this
//! command, with the words typed so far, and it answers from the files alone.
//! It runs on every TAB, so it never talks to the head, never writes, and
//! prints nothing for anything it cannot read.

use std::path::Path;

use clap::{Arg, Command};

use crate::cli::{one_line, task_note};
use crate::config::Paths;
use crate::config::flock::Flock;
use crate::store::{Store, TaskFilter};
use crate::task::LIVE_STATES;

/// What a name slot takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Job,
    Flock,
    Machine,
    Task,
    Connector,
}

/// The kind of name the argument `id` of the subcommand at `path` (canonical
/// names, `pastor` left out) takes, if it takes one that already exists.
pub fn kind_of(path: &[&str], id: &str) -> Option<Kind> {
    match (path, id) {
        // A new flock's or machine's name is the user's to choose.
        ([_, "add"], "name") => None,
        (_, "flock") => Some(Kind::Flock),
        (_, "machine") => Some(Kind::Machine),
        (_, "task") => Some(Kind::Task),
        (_, "job") => Some(Kind::Job),
        (["connector", ..], "id") => Some(Kind::Connector),
        (["job", _], "name") => Some(Kind::Job),
        (["flock", _], "name") => Some(Kind::Flock),
        (["machine", _], "name") => Some(Kind::Machine),
        _ => None,
    }
}

/// The kind of name `words` ends in: the words after `pastor`, the last one
/// being the word under the cursor (empty for a fresh one). `None` when that
/// word is a subcommand, a flag, a value that is not a name, or a slot that
/// is already filled. `root` is the whole command tree.
pub fn slot(root: &Command, words: &[String]) -> Option<Kind> {
    let (current, done) = match words.split_last() {
        Some((c, d)) => (c.as_str(), d),
        None => ("", &[][..]),
    };
    let mut node = root;
    let mut path: Vec<&str> = Vec::new();
    let mut positionals = 0;
    let mut pending: Option<&Arg> = None;
    let mut only_positionals = false;
    for word in done {
        if let Some(arg) = pending.take() {
            // bash splits `--flock=home` into `--flock`, `=` and `home`.
            if word == "=" {
                pending = Some(arg);
            }
            continue;
        }
        if !only_positionals && word == "--" {
            only_positionals = true;
        } else if !only_positionals && word.starts_with('-') && word.len() > 1 {
            if let Some(arg) = option(node, word)
                && arg.get_action().takes_values()
                && !word.contains('=')
            {
                pending = Some(arg);
            }
        } else if positionals == 0
            && !only_positionals
            && let Some(sub) = node.find_subcommand(word)
        {
            node = sub;
            path.push(sub.get_name());
        } else {
            positionals += 1;
        }
    }
    let arg = match pending {
        Some(arg) => arg,
        None if !only_positionals && current.starts_with('-') && current.len() > 1 => {
            // `--flock=<TAB>`: the value of the option before the `=`.
            let (flag, _) = current.split_once('=')?;
            option(node, flag).filter(|a| a.get_action().takes_values())?
        }
        None => node.get_positionals().nth(positionals)?,
    };
    kind_of(&path, arg.get_id().as_str())
}

/// The option `word` names on `node`: `--long`, `--long=value` or `-s`.
fn option<'a>(node: &'a Command, word: &str) -> Option<&'a Arg> {
    if let Some(long) = word.strip_prefix("--") {
        let long = long.split_once('=').map_or(long, |(l, _)| l);
        node.get_arguments().find(|a| a.get_long() == Some(long))
    } else {
        let short = word.strip_prefix('-')?.chars().next()?;
        node.get_arguments().find(|a| a.get_short() == Some(short))
    }
}

/// Every long option in the tree that takes a name, sorted, once each: fish
/// needs them listed, since after `--flock` it asks only that option's
/// completions.
pub fn name_options(root: &Command) -> Vec<String> {
    fn walk(node: &Command, path: &mut Vec<String>, out: &mut Vec<String>) {
        let names: Vec<&str> = path.iter().map(String::as_str).collect();
        for arg in node.get_arguments() {
            if let Some(long) = arg.get_long()
                && kind_of(&names, arg.get_id().as_str()).is_some()
            {
                out.push(long.to_string());
            }
        }
        for sub in node.get_subcommands() {
            path.push(sub.get_name().to_string());
            walk(sub, path, out);
            path.pop();
        }
    }
    let mut out = Vec::new();
    walk(root, &mut Vec::new(), &mut out);
    out.sort();
    out.dedup();
    out
}

/// The names of `kind`, each with a short description or none, in the order
/// to offer them. Anything unreadable is no names.
pub fn names(paths: &Paths, kind: Kind) -> Vec<(String, Option<String>)> {
    match kind {
        Kind::Job => dir_names(&paths.jobs_dir(), Some("toml")),
        Kind::Connector => dir_names(&paths.connectors_dir(), None),
        Kind::Flock => {
            let Ok(flock) = Flock::load(&paths.flock_file()) else {
                return Vec::new();
            };
            let default = flock.default_flock();
            flock
                .flock_names()
                .into_iter()
                .map(|n| (n.to_string(), (n == default).then(|| "default".into())))
                .collect()
        }
        Kind::Machine => {
            let Ok(flock) = Flock::load(&paths.flock_file()) else {
                return Vec::new();
            };
            flock
                .machines
                .iter()
                .map(|m| (m.name.clone(), Some(flock.flock_of(m).to_string())))
                .collect()
        }
        Kind::Task => tasks(paths),
    }
}

/// Live tasks first, then finished ones, newest first in each; the note
/// `task list` shows is the description.
fn tasks(paths: &Paths) -> Vec<(String, Option<String>)> {
    let Ok(store) = Store::open_read_only(&paths.db_file()) else {
        return Vec::new();
    };
    let Ok(mut tasks) = store.list_tasks(&TaskFilter::default()) else {
        return Vec::new();
    };
    tasks.sort_by_key(|t| (!LIVE_STATES.contains(&t.state), std::cmp::Reverse(t.id)));
    tasks
        .iter()
        .map(|t| (t.display_id(), Some(task_note(t))))
        .collect()
}

/// The entries of `dir`, sorted, hidden ones left out; with `ext`, only files
/// with that extension, named by their stem.
fn dir_names(dir: &Path, ext: Option<&str>) -> Vec<(String, Option<String>)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(|e| {
            let path = e.ok()?.path();
            let name = match ext {
                Some(ext) => {
                    (path.extension()?.to_str()? == ext).then_some(())?;
                    path.file_stem()?.to_str()?
                }
                None => path.file_name()?.to_str()?,
            };
            (!name.starts_with('.')).then(|| name.to_string())
        })
        .collect();
    out.sort();
    out.into_iter().map(|n| (n, None)).collect()
}

/// One line per name. fish shows the text after a tab next to the name;
/// every other shell gets the name alone. Both are escaped, as `task list`
/// escapes its cells, so an item's text cannot reach the terminal raw.
pub fn render(names: &[(String, Option<String>)], descriptions: bool) -> String {
    let mut out = String::new();
    for (name, desc) in names {
        out.push_str(&one_line(name));
        if descriptions && let Some(desc) = desc {
            out.push('\t');
            out.push_str(&one_line(desc));
        }
        out.push('\n');
    }
    out
}

/// Appended to `pastor completions fish`. The condition runs pastor once and
/// keeps its answer for the argument list; it fails, leaving the static
/// completions alone, where the word takes no name.
pub fn fish_hook(root: &Command) -> String {
    let longs: String = name_options(root)
        .iter()
        .map(|l| format!(" -l {l}"))
        .collect();
    format!(
        r#"
# Names (jobs, flocks, machines, tasks, connectors) come from pastor itself.
function __fish_pastor_names
    set -g __fish_pastor_names (pastor __complete fish -- (commandline -opc)[2..] (commandline -ct) 2>/dev/null)
end

complete -c pastor -n __fish_pastor_names -k -f -a '(printf "%s\n" $__fish_pastor_names)'
complete -c pastor -n __fish_pastor_names{longs} -r -k -f -a '(printf "%s\n" $__fish_pastor_names)'
"#
    )
}

/// Appended to `pastor completions bash`: `_pastor_names` answers name slots
/// and hands every other word to the generated `_pastor`.
pub const BASH_HOOK: &str = r#"
# Names (jobs, flocks, machines, tasks, connectors) come from pastor itself.
_pastor_names() {
    local cur="${COMP_WORDS[COMP_CWORD]}" names
    if names=$(pastor __complete bash -- "${COMP_WORDS[@]:1:COMP_CWORD-1}" "${cur}" 2>/dev/null); then
        [[ ${cur} == "=" ]] && cur=""
        COMPREPLY=( $(compgen -W "${names}" -- "${cur}") )
        return 0
    fi
    _pastor "$@"
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _pastor_names -o nosort -o bashdefault -o default pastor
else
    complete -F _pastor_names -o bashdefault -o default pastor
fi
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_escapes_names_and_notes_and_drops_notes_for_bash() {
        let names = vec![
            (
                "t-1".to_string(),
                Some("fix\u{1b}]52;c;x\u{7}\tit".to_string()),
            ),
            ("odd\nname".to_string(), None),
        ];
        assert_eq!(
            render(&names, true),
            "t-1\tfix\\x1b]52;c;x\\x07\\tit\nodd\\nname\n"
        );
        assert_eq!(render(&names, false), "t-1\nodd\\nname\n");
    }
}
