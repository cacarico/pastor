# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_pastor_global_optspecs
    string join \n skill head= h/help V/version
end

function __fish_pastor_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_pastor_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_pastor_using_subcommand
    set -l cmd (__fish_pastor_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c pastor -n "__fish_pastor_needs_command" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_needs_command" -l skill -d 'Print the agent skill (SKILL.md) for this version and exit'
complete -c pastor -n "__fish_pastor_needs_command" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_needs_command" -s V -l version -d 'Print version'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "serve" -d 'Run the daemon in the background: scheduler, machine channels, dispatch. With a head set on another machine, run headless: this machine\'s jobs and hooks, and the head\'s tasks for it as a pull machine'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "task" -d 'Manage tasks'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "queue" -d 'The queued tasks in the order they will start, and why each waits; move one'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "status" -d 'The fleet on one screen: the head, machines up and down, live tasks by state, failing jobs, exhausted accounts'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "machine" -d 'Manage the machines and which flock each is in'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "flock" -d 'Manage the flocks: named groups of machines that tasks and jobs target'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "tick" -d 'Run one scheduler pass now and report what it did'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "job" -d 'Manage jobs (files in ~/.config/pastor/jobs/). With a head elsewhere, a job whose file is here is this machine\'s; the rest are the head\'s'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "orchestrator" -d 'Manage orchestrators (files in ~/.config/pastor/orchestrators/): the head runs a pre script on a schedule and starts an agent with the orchestrator role for what it prints, or keeps one agent running through set hours'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "config" -d 'pastor.toml: the head\'s settings and the task defaults'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "completions" -d 'Print a shell completion script (fish, bash, zsh, ...) to stdout'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "events" -d 'Show the events log (task, job and machine events)'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "watch" -d 'Print one line per change to act on: tasks, failing jobs, the head, connector lines'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "setup" -d 'Install pastor or herdr as a user service (systemd, or launchd on macOS)'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "bootstrap" -d 'Set this machine up for a flock, as the login user: keys, client.toml, services, harnesses'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "fleet" -d 'The fleet as a whole: pastor updates'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "connector" -d 'Install, link, list and try out connectors'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "bundle" -d 'Bundles of skills, settings, MCP servers and instructions for agents\' homes'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "workdir" -d 'Sets of files placed in an agent\'s working directory before it starts'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "profile" -d 'Permission profiles: named tool allow and deny lists, built in and from pastor.toml'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "limit" -d 'The accounts that ran out of usage, which no new task starts on until they reset'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "agent" -d 'Agents on machines: check that one is signed in'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "login" -d 'Sign an agent in on a machine: open its login in a pane there, show the URL and code, and clear its signed-out mark once it passes'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "trust" -d 'The repos whose folder-trust prompt pastor answers on each machine'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "join" -d 'Join a head with a one-time token as a pull machine, or make and list the tokens on the head'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "bridge" -d 'Pass request lines from stdin to this machine\'s head; for a remote CLI over ssh'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "inbox" -d 'Talk to a Claude session\'s inbox socket on this machine'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "head" -d 'Which head this CLI uses: this machine\'s, or one on another machine over ssh'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "leave" -d 'Take from this host what pastor put there for itself: its marked authorized_keys lines and its own key files in the state directory, nothing else'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand serve; and not __fish_seen_subcommand_from stop status help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand serve; and not __fish_seen_subcommand_from stop status help" -s f -l foreground -d 'Run in this terminal until a signal, logging to stderr; what a service runs'
complete -c pastor -n "__fish_pastor_using_subcommand serve; and not __fish_seen_subcommand_from stop status help" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c pastor -n "__fish_pastor_using_subcommand serve; and not __fish_seen_subcommand_from stop status help" -f -a "stop" -d 'Stop the pastor serve running here (SIGTERM); agents keep running'
complete -c pastor -n "__fish_pastor_using_subcommand serve; and not __fish_seen_subcommand_from stop status help" -f -a "status" -d 'Whether pastor serve runs here: head or headless, pid, version, service, log'
complete -c pastor -n "__fish_pastor_using_subcommand serve; and not __fish_seen_subcommand_from stop status help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand serve; and __fish_seen_subcommand_from stop" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand serve; and __fish_seen_subcommand_from stop" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand serve; and __fish_seen_subcommand_from status" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand serve; and __fish_seen_subcommand_from status" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand serve; and __fish_seen_subcommand_from status" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand serve; and __fish_seen_subcommand_from help" -f -a "stop" -d 'Stop the pastor serve running here (SIGTERM); agents keep running'
complete -c pastor -n "__fish_pastor_using_subcommand serve; and __fish_seen_subcommand_from help" -f -a "status" -d 'Whether pastor serve runs here: head or headless, pid, version, service, log'
complete -c pastor -n "__fish_pastor_using_subcommand serve; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune sweep send done help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune sweep send done help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune sweep send done help" -f -a "run" -d 'Create a one-off task and dispatch it'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune sweep send done help" -f -a "list" -d 'List live tasks across the flock; --all adds finished ones'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune sweep send done help" -f -a "describe" -d 'One task in full: state, machine, agent, the models it moved through on usage limits, prompt, error, summary'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune sweep send done help" -f -a "read" -d 'Read recent output from a task\'s pane'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune sweep send done help" -f -a "attach" -d 'Attach to a task\'s agent terminal (ctrl+b q detaches); a closed Claude task\'s session reopens in a new pane'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune sweep send done help" -f -a "retry" -d 'Re-dispatch a failed or stale task as a new task (retry_of points back)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune sweep send done help" -f -a "priority" -d 'Put a queued task at another level: low, normal, high or critical'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune sweep send done help" -f -a "close" -d 'Close tasks\' panes (and with --remove-worktree their worktrees), or orphaned agents'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune sweep send done help" -f -a "prune" -d 'Delete old finished tasks; their items stay seen'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune sweep send done help" -f -a "sweep" -d 'List the worktrees pastor kept for tasks that ended, and with --yes remove those with nothing in them to lose'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune sweep send done help" -f -a "send" -d 'Type text or press keys in a live task\'s agent, to answer what it is waiting on'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune sweep send done help" -f -a "done" -d 'Mark a task done, its pane to close after close_done_after; an agent may end its own'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune sweep send done help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l prompt-file -d 'Read the prompt from this file on this machine (\'-\' for stdin); it spares long prompts the shell\'s quoting' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l repo -d 'The repo the agent works in: a path on the machine that runs the task, not on this one' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l flock -d 'Only this flock\'s machines take the task (default: the flock of --machine, else the default flock)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l machine -d 'Run it on this machine (a name from flock.toml) instead of any free one' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l agent -d 'The agent command to start, like claude or codex (default: the machine\'s, else its flock\'s, else `[defaults]`, else claude)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l agent-arg -d 'One argument for the agent; repeat it, in order, for more. Replaces the flock\'s and `[defaults]` agent_args. The next word is always the value, dashes and all' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l model -d 'Run this model, a name from `[models]` in pastor.toml; its args go before the agent\'s (default: the flock\'s, else the machine\'s, else `[defaults] model`, else none)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l fallback -d 'The models the task may fall back to, in order: names from `[models]` in pastor.toml, comma separated (default: the machine\'s, else the flock\'s, else `[defaults] fallback`, else none)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l priority -d 'Queue at this level: low, normal, high or critical; dispatch takes higher levels first (default: the flock\'s, else the pinned machine\'s, else `[defaults] priority`, else normal)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l profile -d 'Run under this permission profile, built in or from `[profiles]` in pastor.toml: a Claude agent gets its allow and deny lists and never asks (default: the flock\'s, else the machine\'s, else `[defaults] profile`, else none)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l branch -d 'Branch for the worktree (needs --worktree; a plain workspace has no branch)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l tag -d 'Only a machine with this tag takes the task; repeat for more, and it needs them all' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l load -d 'The kind of work the task is (`rust`, `light`): only a machine that takes this load in flock.toml places it (default: its agent\'s `load`, else its repo\'s under `[repos]`, else none)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l timeout -d 'Mark the task stale once it has run this long (30m, 2h; default: the flock\'s, else `[defaults] timeout`)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l label -d 'Label template of the workspace pastor makes for the task, with {{ task.id }}, {{ flock }}, {{ machine }}, {{ job }} and {{ item.key }} (default: the flock\'s `label`, else `[defaults] label`, else {{ flock }}/{{ task.id }}). The agent stays t-N' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l workdir-set -d 'A workdir set to place in the agent\'s folder before each start: a JSON or TOML file, a directory with a pastor-workdir.toml, or \'-\' for stdin. Its profile must be the task\'s, or none (it takes the task\'s); a failed place fails the start (workdir_failed)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l place -d 'Where the agent\'s pane goes: repo (under the repo it works on), own (its own workspace), pastor (the `pastor` workspace) or pane:<workspace> (default: the flock\'s, else `[defaults] place`, else repo)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l role -d 'What the agent may change through the head: agent (read, and end its own task) or orchestrator (also run, retry, send to and close tasks and enable and disable jobs). Only a person may start an orchestrator, never a task' -r -f -a "agent\t'Reads, and `task done` for its own task; everything else is refused'
orchestrator\t'Also runs, retries, types into and closes tasks and enables and disables jobs (`IpcRequest::orchestrator_may`). Only a person makes one: `task run --role orchestrator` from outside any task'"
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l summary -d 'Ask the agent for a summary when it finishes (ask), also fail the task if it stops without one (require), or neither (off) (default: the flock\'s `summary`, else `[defaults] summary`, else ask)' -r -f -a "ask\t'Add the line that asks for a summary to the prompt'
require\t'Ask, and fail the task if its agent stops without one'
off\t'Add nothing and require nothing'"
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l description -d 'One line on what the task is about, for `task list --wide` and `describe` (default: the prompt\'s first line)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l no-fallback -d 'Fall back to no other model, whatever the machine, flock or `[defaults]` say'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l preempt -d 'A critical task only: on a full machine, pause the newest low Claude task there (its session resumes when a slot frees) and take its slot'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l now -d 'Start at once on --machine (which it needs), skipping the queue and running past that machine\'s max_agents, job slots, burst and flock number; meant for a person at the CLI, and refused from a task or an orchestrator as a guard against mistakes, not a boundary'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l worktree -d 'A git worktree per task, branched from --repo (so it needs --repo)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l keep-pane -d 'Keep the agent\'s pane open once the task is done, failed or stale, until `pastor task close`, so you can go on talking to the agent; a kept done task holds no slot (default: the flock\'s `keep_pane`, else `[defaults] keep_pane`, else no)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l remove-worktree -d 'Remove the task\'s worktree once it ends, failed or stale too, not only done, as its pane closes; its branch stays, and a dirty checkout is kept with a note (default: the flock\'s `remove_worktree`, else `[defaults] remove_worktree`, else no)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l no-fetch -d 'Do not fetch the repo\'s remote before the task\'s worktree is made (default: fetch it; a fetch that fails warns, and the worktree is made from the repo as it is)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l job -d 'Only tasks from this job (omit for one-off `run` tasks)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l flock -d 'Only tasks of this flock' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l machine -d 'Only tasks on this machine' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l blocked -d 'Only blocked tasks, needing a human'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l done -d 'Only done tasks'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l all -d 'Every task, finished ones too (done, failed, stale, closed)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -s w -l wide -d 'Add RESULT and DESCRIPTION columns, the second cut to the terminal\'s width'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON array of full task records'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from describe" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from describe" -l all-summaries -d 'Show every round\'s summary, not only the last'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from describe" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from describe" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from read" -l lines -d 'How many lines from the bottom of the pane' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from read" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from read" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from attach" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from attach" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from retry" -l place -d 'Where the new task\'s pane goes instead of the old one\'s: repo, own, pastor or pane:<workspace>' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from retry" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from retry" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from retry" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from priority" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from priority" -l preempt -d 'Critical only: let the task pause the newest low Claude task on a full machine to start; without it the task\'s flag goes'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from priority" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from priority" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from close" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from close" -l remove-worktree -d 'Remove each task\'s worktree too (refused if it has uncommitted changes)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from close" -l json -d 'Print as a JSON object, or an array of them for several tasks'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from close" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l older-than -d 'Only tasks that finished longer ago than this (30m, 12h, 3d)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l done -d 'Prune done tasks'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l failed -d 'Prune failed tasks'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l closed -d 'Prune closed tasks'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from sweep" -l machine -d 'Only this machine\'s worktrees' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from sweep" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from sweep" -l yes -d 'Remove what can be removed; without it nothing is removed and the worktrees are only listed'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from sweep" -l dry-run -d 'List and remove nothing (this is what no flag does too)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from sweep" -l json -d 'Print as a JSON object, {entries, failed}'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from sweep" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -l key -d 'A named key to press after the text (Enter, Down, esc, ctrl+c); repeat for more, in order' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -l pick -d 'Answer the picker on screen by the text of one of its options (case does not matter): the arrows to it, then Enter' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -l no-enter -d 'Type the text without pressing Enter after it'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -l trust -d 'Accept the agent\'s folder-trust prompt with its trust keys, and trust the task\'s repo on its machine from now on'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from done" -l summary -d 'What you did, its first line the outcome: done, partial, blocked or nothing to do (kept to 2,000 characters)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from done" -l summary-file -d 'Read the summary from a file, or - for stdin' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from done" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from done" -l current -d 'Do the whole end of a task in one step, for the task this pane runs (from PASTOR_TASK): check its worktree, if it has one, has nothing uncommitted, push its branch if it is not pushed yet, send the summary, and print DONE last. A dirty worktree or a failed push leaves the task as it was, with a clear error, instead of ending it'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from done" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from done" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "run" -d 'Create a one-off task and dispatch it'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "list" -d 'List live tasks across the flock; --all adds finished ones'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "describe" -d 'One task in full: state, machine, agent, the models it moved through on usage limits, prompt, error, summary'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "read" -d 'Read recent output from a task\'s pane'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "attach" -d 'Attach to a task\'s agent terminal (ctrl+b q detaches); a closed Claude task\'s session reopens in a new pane'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "retry" -d 'Re-dispatch a failed or stale task as a new task (retry_of points back)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "priority" -d 'Put a queued task at another level: low, normal, high or critical'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "close" -d 'Close tasks\' panes (and with --remove-worktree their worktrees), or orphaned agents'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "prune" -d 'Delete old finished tasks; their items stay seen'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "sweep" -d 'List the worktrees pastor kept for tasks that ended, and with --yes remove those with nothing in them to lose'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "send" -d 'Type text or press keys in a live task\'s agent, to answer what it is waiting on'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "done" -d 'Mark a task done, its pane to close after close_done_after; an agent may end its own'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand queue; and not __fish_seen_subcommand_from move help" -l flock -d 'Only the tasks waiting in this flock' -r
complete -c pastor -n "__fish_pastor_using_subcommand queue; and not __fish_seen_subcommand_from move help" -l machine -d 'Only the tasks pinned to this machine' -r
complete -c pastor -n "__fish_pastor_using_subcommand queue; and not __fish_seen_subcommand_from move help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand queue; and not __fish_seen_subcommand_from move help" -l json -d 'Print as a JSON array'
complete -c pastor -n "__fish_pastor_using_subcommand queue; and not __fish_seen_subcommand_from move help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand queue; and not __fish_seen_subcommand_from move help" -f -a "move" -d 'Put a queued task elsewhere in the queue; it takes the level of where it lands'
complete -c pastor -n "__fish_pastor_using_subcommand queue; and not __fish_seen_subcommand_from move help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand queue; and __fish_seen_subcommand_from move" -l before -d 'Just ahead of this queued task' -r
complete -c pastor -n "__fish_pastor_using_subcommand queue; and __fish_seen_subcommand_from move" -l after -d 'Just behind this queued task' -r
complete -c pastor -n "__fish_pastor_using_subcommand queue; and __fish_seen_subcommand_from move" -l to -d 'At this position, as `pastor queue` numbers it (1 is first)' -r
complete -c pastor -n "__fish_pastor_using_subcommand queue; and __fish_seen_subcommand_from move" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand queue; and __fish_seen_subcommand_from move" -l top -d 'First in the queue, lifted to the first task\'s level if that is higher'
complete -c pastor -n "__fish_pastor_using_subcommand queue; and __fish_seen_subcommand_from move" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand queue; and __fish_seen_subcommand_from move" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand queue; and __fish_seen_subcommand_from help" -f -a "move" -d 'Put a queued task elsewhere in the queue; it takes the level of where it lands'
complete -c pastor -n "__fish_pastor_using_subcommand queue; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand status" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand status" -l probe -d 'With no head running, probe each machine over ssh and read tasks, jobs and limits from the files and store here, instead of failing'
complete -c pastor -n "__fish_pastor_using_subcommand status" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand status" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add bootstrap remove move cordon uncordon list describe check install open authorized-key help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add bootstrap remove move cordon uncordon list describe check install open authorized-key help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add bootstrap remove move cordon uncordon list describe check install open authorized-key help" -f -a "add" -d 'Add a machine to flock.toml, reached over ssh, locally or by a command'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add bootstrap remove move cordon uncordon list describe check install open authorized-key help" -f -a "bootstrap" -d 'Set an ssh machine up from the head, as its login user, or finish a setup that stopped; pins its host key on first use and never replaces it'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add bootstrap remove move cordon uncordon list describe check install open authorized-key help" -f -a "remove" -d 'Remove a machine from flock.toml; tasks already on it keep their rows'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add bootstrap remove move cordon uncordon list describe check install open authorized-key help" -f -a "move" -d 'Take a machine out of every flock and put it in this one; tasks already on it stay there'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add bootstrap remove move cordon uncordon list describe check install open authorized-key help" -f -a "cordon" -d 'Take no new task on a machine until it is uncordoned; the tasks it runs go on'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add bootstrap remove move cordon uncordon list describe check install open authorized-key help" -f -a "uncordon" -d 'Let a cordoned machine take tasks again, and place what waited for it'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add bootstrap remove move cordon uncordon list describe check install open authorized-key help" -f -a "list" -d 'A line about the head, then each machine: host, flock, channel, herdr, agents'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add bootstrap remove move cordon uncordon list describe check install open authorized-key help" -f -a "describe" -d 'One machine in full: host, flock, channel, versions, agents, recent errors'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add bootstrap remove move cordon uncordon list describe check install open authorized-key help" -f -a "check" -d 'Run a machine\'s provisioner\'s checks there and say what it is missing for the agents that can land on it; changes nothing, and exits 1 if a check failed'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add bootstrap remove move cordon uncordon list describe check install open authorized-key help" -f -a "install" -d 'Install or update each harness the machine\'s flock pins, and herdr when pastor.toml pins it, to the pinned version, as the machine\'s user; exits 1 if one could not be'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add bootstrap remove move cordon uncordon list describe check install open authorized-key help" -f -a "open" -d 'Open the full herdr UI on a machine'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add bootstrap remove move cordon uncordon list describe check install open authorized-key help" -f -a "authorized-key" -d 'Print the authorized_keys line that lets a machine\'s agents reach this head; with --write, put it there on its marked line'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add bootstrap remove move cordon uncordon list describe check install open authorized-key help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l command -d 'Developer option: the bridge command as one string, split on whitespace (`--command "fake-herdr --connect /tmp/h.sock"`). Words containing spaces go in flock.toml by hand' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l session -d 'The herdr session on the machine that agents run in' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l max-agents -d 'How many tasks it runs at once' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l job-slots -d 'Extra slots only tasks from jobs take, on top of --max-agents; 0 for none' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l burst -d 'How many past --max-agents a critical task may start; 0 for none' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l tag -d 'A label a task\'s --tag can ask for; repeat for more' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l flock -d 'The flock it joins (default: the default flock)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l description -d 'One line on what the machine is for, for `machine list --wide`' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l agent-key -d 'The public key its agents reach the head with (a .pub file, or - for stdin): its marked line goes in this host\'s authorized_keys, run on the head' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l host-key -d 'With --bootstrap: the host key the machine must offer, as a SHA256: fingerprint or an ssh-ed25519 key; unset, the key is trusted the first time it is seen' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l local -d 'This machine itself, through herdr\'s local socket; no ssh'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l herdr -d 'Also save it in herdr\'s sidebar (runs `herdr machine add`)'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l dry-run -d 'Change nothing: say what would be added, and the authorized_keys line with --agent-key'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l bootstrap -d 'Set the machine up over ssh from the head, as its login user, after adding it: pin its host key, copy pastor and herdr, run pastor bootstrap there (needs a running head and head_address)'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from bootstrap" -l host-key -d 'The host key the machine must offer, as a SHA256: fingerprint or an ssh-ed25519 key' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from bootstrap" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from bootstrap" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from remove" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from remove" -l herdr -d 'Also remove herdr\'s saved machine with this label'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from remove" -l dry-run -d 'Change nothing: list what would go (its flock.toml entry, its marked authorized_keys lines, its known_hosts entry, its bootstrap state)'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from remove" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from move" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from move" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from cordon" -l reason -d 'Why, one line; `machine list` and `machine describe` show it' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from cordon" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from cordon" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from uncordon" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from uncordon" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from list" -l flock -d 'Only the machines of this flock' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from list" -s w -l wide -d 'Add a DESCRIPTION column, cut to the terminal\'s width'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON object, {head, machines}: the head\'s row, then the machines'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from describe" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from describe" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from describe" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from check" -l provisioner -d 'Use this provisioner instead of the machine\'s own or the first that fits it' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from check" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from check" -l json -d 'Print as a JSON object, {machine, provisioner, version, agents, rows}'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from check" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from install" -l agent -d 'Only the harness this agent runs' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from install" -l kind -d 'Only this kind of harness, such as claude, or herdr' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from install" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from install" -l dry-run -d 'Say what would change, and change nothing'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from install" -l json -d 'Print as a JSON object, {machine, dry_run, rows}'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from install" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from open" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from open" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from authorized-key" -l key -d 'The public key of the machine\'s user: a .pub file, or - for stdin' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from authorized-key" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from authorized-key" -l shepherd -d 'The line for the machine\'s shepherd instead of its agents\': `bridge --shepherd`, for a pull machine\'s own key'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from authorized-key" -l write -d 'Write the machine\'s marked line in this host\'s authorized_keys, replacing the one it had; run on the head'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from authorized-key" -l dry-run -d 'With --write: print the line and the file, and change nothing'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from authorized-key" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "add" -d 'Add a machine to flock.toml, reached over ssh, locally or by a command'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "bootstrap" -d 'Set an ssh machine up from the head, as its login user, or finish a setup that stopped; pins its host key on first use and never replaces it'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "remove" -d 'Remove a machine from flock.toml; tasks already on it keep their rows'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "move" -d 'Take a machine out of every flock and put it in this one; tasks already on it stay there'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "cordon" -d 'Take no new task on a machine until it is uncordoned; the tasks it runs go on'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "uncordon" -d 'Let a cordoned machine take tasks again, and place what waited for it'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "list" -d 'A line about the head, then each machine: host, flock, channel, herdr, agents'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "describe" -d 'One machine in full: host, flock, channel, versions, agents, recent errors'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "check" -d 'Run a machine\'s provisioner\'s checks there and say what it is missing for the agents that can land on it; changes nothing, and exits 1 if a check failed'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "install" -d 'Install or update each harness the machine\'s flock pins, and herdr when pastor.toml pins it, to the pinned version, as the machine\'s user; exits 1 if one could not be'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "open" -d 'Open the full herdr UI on a machine'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "authorized-key" -d 'Print the authorized_keys line that lets a machine\'s agents reach this head; with --write, put it there on its marked line'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add join leave remove default edit describe help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add join leave remove default edit describe help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add join leave remove default edit describe help" -f -a "list" -d 'Every flock: default or not, its machines, live agents, queued tasks'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add join leave remove default edit describe help" -f -a "add" -d 'Declare a flock; with --default, new tasks and jobs go to it'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add join leave remove default edit describe help" -f -a "join" -d 'Put a machine in a flock, or change its number there; it stays in its other flocks'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add join leave remove default edit describe help" -f -a "leave" -d 'Take a machine out of a flock; out of its last one it is in the default flock'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add join leave remove default edit describe help" -f -a "remove" -d 'Remove a flock; refused while it has machines or queued tasks, or is the default'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add join leave remove default edit describe help" -f -a "default" -d 'The flock that new tasks and jobs go to'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add join leave remove default edit describe help" -f -a "edit" -d 'Open flock.toml in $VISUAL or $EDITOR; save it only once it is valid'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add join leave remove default edit describe help" -f -a "describe" -d 'One flock in full: default or not, its agent, machines, live tasks'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add join leave remove default edit describe help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from list" -s w -l wide -d 'Add a DESCRIPTION column, cut to the terminal\'s width'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON array'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -l description -d 'One line on what the flock is for, for `flock list --wide`' -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -l default -d 'Make it the default flock too'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from join" -l max -d 'At most this many of the flock\'s tasks on the machine; 0 drains it (default: its max_agents, or its number there already)' -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from join" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from join" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from leave" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from leave" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from remove" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from remove" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from default" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from default" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from default" -f -a "show" -d 'Print the default flock'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from default" -f -a "set" -d 'Make another flock the default; machines stay in their flocks'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from default" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from edit" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from edit" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from describe" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from describe" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from describe" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "list" -d 'Every flock: default or not, its machines, live agents, queued tasks'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "add" -d 'Declare a flock; with --default, new tasks and jobs go to it'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "join" -d 'Put a machine in a flock, or change its number there; it stays in its other flocks'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "leave" -d 'Take a machine out of a flock; out of its last one it is in the default flock'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "remove" -d 'Remove a flock; refused while it has machines or queued tasks, or is the default'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "default" -d 'The flock that new tasks and jobs go to'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "edit" -d 'Open flock.toml in $VISUAL or $EDITOR; save it only once it is valid'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "describe" -d 'One flock in full: default or not, its agent, machines, live tasks'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand tick" -l job -d 'Only this job, and run it whether or not it is due' -r
complete -c pastor -n "__fish_pastor_using_subcommand tick" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand tick" -l dry-run -d 'Run connectors and show what would be created; write nothing'
complete -c pastor -n "__fish_pastor_using_subcommand tick" -l json -d 'Print as a JSON array, one entry per job run'
complete -c pastor -n "__fish_pastor_using_subcommand tick" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload edit describe help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload edit describe help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload edit describe help" -f -a "list" -d 'Every job file: schedule, enabled, last run, next run, last result. With a head elsewhere, the head\'s jobs, then this machine\'s'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload edit describe help" -f -a "enable" -d 'Enable a job file'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload edit describe help" -f -a "disable" -d 'Disable a job file'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload edit describe help" -f -a "run" -d 'Fire a job now, ignoring its schedule and `enabled`; it starts once a run already going has finished'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload edit describe help" -f -a "reload" -d 'Re-read the job files, flock.toml and pastor.toml now instead of at the next tick'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload edit describe help" -f -a "edit" -d 'Open a job file in $VISUAL or $EDITOR; save it only once it is valid'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload edit describe help" -f -a "describe" -d 'One job in full: schedule, connector, dispatch, last runs, recent tasks'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload edit describe help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from list" -s w -l wide -d 'Add a DESCRIPTION column, cut to the terminal\'s width'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON array; with a head elsewhere, each job says `where` it lives (head or shepherd)'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from enable" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from enable" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from disable" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from disable" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from run" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from run" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from reload" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from reload" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from edit" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from edit" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from describe" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from describe" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from describe" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from help" -f -a "list" -d 'Every job file: schedule, enabled, last run, next run, last result. With a head elsewhere, the head\'s jobs, then this machine\'s'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from help" -f -a "enable" -d 'Enable a job file'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from help" -f -a "disable" -d 'Disable a job file'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from help" -f -a "run" -d 'Fire a job now, ignoring its schedule and `enabled`; it starts once a run already going has finished'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from help" -f -a "reload" -d 'Re-read the job files, flock.toml and pastor.toml now instead of at the next tick'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from help" -f -a "edit" -d 'Open a job file in $VISUAL or $EDITOR; save it only once it is valid'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from help" -f -a "describe" -d 'One job in full: schedule, connector, dispatch, last runs, recent tasks'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and not __fish_seen_subcommand_from list describe run start stop enable disable note help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and not __fish_seen_subcommand_from list describe run start stop enable disable note help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and not __fish_seen_subcommand_from list describe run start stop enable disable note help" -f -a "list" -d 'Every orchestrator file: kind, state, schedule, last and next run, its last agent'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and not __fish_seen_subcommand_from list describe run start stop enable disable note help" -f -a "describe" -d 'One orchestrator in full: its settings, note, last runs with the lines each pre script printed, and recent events'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and not __fish_seen_subcommand_from list describe run start stop enable disable note help" -f -a "run" -d 'Run a scheduled orchestrator now, ignoring its schedule and `enabled`; it starts once a run already going has finished, and is skipped while its last agent works'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and not __fish_seen_subcommand_from list describe run start stop enable disable note help" -f -a "start" -d 'Start a session orchestrator now, inside its hours or not, `enabled` or not; it stops at the next hours.stop'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and not __fish_seen_subcommand_from list describe run start stop enable disable note help" -f -a "stop" -d 'Stop a session orchestrator: its agent gets a last message and is closed after stop_grace; it does not start on its hours again before they next end'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and not __fish_seen_subcommand_from list describe run start stop enable disable note help" -f -a "enable" -d 'Enable an orchestrator file'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and not __fish_seen_subcommand_from list describe run start stop enable disable note help" -f -a "disable" -d 'Disable an orchestrator file; an agent it started keeps running'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and not __fish_seen_subcommand_from list describe run start stop enable disable note help" -f -a "note" -d 'Keep the orchestrator\'s handover note (4 KiB at most), which every agent it starts gets in its prompt; the last one wins, and an empty one removes it'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and not __fish_seen_subcommand_from list describe run start stop enable disable note help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from list" -s w -l wide -d 'Add a DESCRIPTION column, cut to the terminal\'s width'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON array'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from describe" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from describe" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from describe" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from run" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from run" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from start" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from start" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from stop" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from stop" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from enable" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from enable" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from disable" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from disable" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from note" -l name -d 'The orchestrator, for a person; its own agent and scripts may leave it out' -r
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from note" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from note" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from help" -f -a "list" -d 'Every orchestrator file: kind, state, schedule, last and next run, its last agent'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from help" -f -a "describe" -d 'One orchestrator in full: its settings, note, last runs with the lines each pre script printed, and recent events'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from help" -f -a "run" -d 'Run a scheduled orchestrator now, ignoring its schedule and `enabled`; it starts once a run already going has finished, and is skipped while its last agent works'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from help" -f -a "start" -d 'Start a session orchestrator now, inside its hours or not, `enabled` or not; it stops at the next hours.stop'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from help" -f -a "stop" -d 'Stop a session orchestrator: its agent gets a last message and is closed after stop_grace; it does not start on its hours again before they next end'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from help" -f -a "enable" -d 'Enable an orchestrator file'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from help" -f -a "disable" -d 'Disable an orchestrator file; an agent it started keeps running'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from help" -f -a "note" -d 'Keep the orchestrator\'s handover note (4 KiB at most), which every agent it starts gets in its prompt; the last one wins, and an empty one removes it'
complete -c pastor -n "__fish_pastor_using_subcommand orchestrator; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand config; and not __fish_seen_subcommand_from edit help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand config; and not __fish_seen_subcommand_from edit help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand config; and not __fish_seen_subcommand_from edit help" -f -a "edit" -d 'Open pastor.toml in $VISUAL or $EDITOR; save it only once it is valid'
complete -c pastor -n "__fish_pastor_using_subcommand config; and not __fish_seen_subcommand_from edit help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand config; and __fish_seen_subcommand_from edit" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand config; and __fish_seen_subcommand_from edit" -l local -d 'This machine\'s pastor.toml, even with a head on another machine'
complete -c pastor -n "__fish_pastor_using_subcommand config; and __fish_seen_subcommand_from edit" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand config; and __fish_seen_subcommand_from help" -f -a "edit" -d 'Open pastor.toml in $VISUAL or $EDITOR; save it only once it is valid'
complete -c pastor -n "__fish_pastor_using_subcommand config; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand completions" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand completions" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand events" -l task -d 'Only events about this task, like t-12 or 12' -r
complete -c pastor -n "__fish_pastor_using_subcommand events" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand events" -l follow -d 'Keep printing new events as they are written (reads the file, and works with the daemon down; with a remote head, asks it every second)'
complete -c pastor -n "__fish_pastor_using_subcommand events" -l json -d 'Print one JSON record per line, the same shape hooks get on stdin'
complete -c pastor -n "__fish_pastor_using_subcommand events" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand watch" -l name -d 'The watcher\'s name: its cursor is kept under the state dir, so a watcher started again with the same name repeats nothing' -r
complete -c pastor -n "__fish_pastor_using_subcommand watch" -l interval -d 'How often to look, like 30s or 2m' -r
complete -c pastor -n "__fish_pastor_using_subcommand watch" -l connector -d 'Run this connector\'s [watch] command (repeatable); replaces [[watch.connector]] in pastor.toml' -r
complete -c pastor -n "__fish_pastor_using_subcommand watch" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand watch" -l now -d 'Print what needs attention now (blocked, done, failed and stale tasks, failing jobs, a head that does not answer, connector lines) and exit'
complete -c pastor -n "__fish_pastor_using_subcommand watch" -l reset -d 'Forget the cursor and start from the end of the events log'
complete -c pastor -n "__fish_pastor_using_subcommand watch" -l all -d 'Every task state change, not only blocked, done, failed and stale (with --now: every live task too)'
complete -c pastor -n "__fish_pastor_using_subcommand watch" -l json -d 'Print one JSON record per line: kind, the line\'s fields, and line, its text'
complete -c pastor -n "__fish_pastor_using_subcommand watch" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and not __fish_seen_subcommand_from systemd launchd help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand setup; and not __fish_seen_subcommand_from systemd launchd help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and not __fish_seen_subcommand_from systemd launchd help" -f -a "systemd" -d 'Install and manage a systemd user unit: pastor.service, or herdr.service with --herdr'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and not __fish_seen_subcommand_from systemd launchd help" -f -a "launchd" -d 'Install and manage a launchd user agent on macOS: pastor.serve, or pastor.herdr with --herdr'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and not __fish_seen_subcommand_from systemd launchd help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -l herdr -d 'Install herdr.service (the herdr server) instead, for a flock machine'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -l enable -d 'Enable the unit at login'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -l start -d 'Start the unit now'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -l now -d 'With --enable, start the unit now too (`systemctl --user enable --now`)'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -l stop -d 'Stop the unit now'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -s y -l yes -d 'Skip the confirmation prompt; needed when stdin is not a terminal (a script, a task, `ssh host pastor setup systemd`)'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from launchd" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from launchd" -l herdr -d 'Install pastor.herdr (the herdr server) instead, for a flock machine'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from launchd" -l enable -d 'Enable the agent at login; one not loaded yet is loaded, which starts it'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from launchd" -l start -d 'Start the agent now'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from launchd" -l now -d 'With --enable, start the agent now too'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from launchd" -l stop -d 'Stop the agent now'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from launchd" -s y -l yes -d 'Skip the confirmation prompt; needed when stdin is not a terminal (a script, a task, `ssh host pastor setup launchd`)'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from launchd" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from help" -f -a "systemd" -d 'Install and manage a systemd user unit: pastor.service, or herdr.service with --herdr'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from help" -f -a "launchd" -d 'Install and manage a launchd user agent on macOS: pastor.serve, or pastor.herdr with --herdr'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand bootstrap" -l name -d 'The machine\'s name in the head\'s flock.toml (the hostname when unset)' -r
complete -c pastor -n "__fish_pastor_using_subcommand bootstrap" -l head-key -d 'The head\'s public key line, for this user\'s authorized_keys' -r
complete -c pastor -n "__fish_pastor_using_subcommand bootstrap" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand bootstrap" -l json -d 'Print as a JSON object: isolation, each step, the public keys and versions, the check'
complete -c pastor -n "__fish_pastor_using_subcommand bootstrap" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand fleet; and not __fish_seen_subcommand_from update help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand fleet; and not __fish_seen_subcommand_from update help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand fleet; and not __fish_seen_subcommand_from update help" -f -a "update" -d 'Put pastor <version> from the head\'s artifact cache on every machine the head reaches, the head last'
complete -c pastor -n "__fish_pastor_using_subcommand fleet; and not __fish_seen_subcommand_from update help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand fleet; and __fish_seen_subcommand_from update" -l machine -d 'Only this machine, as flock.toml names it' -r
complete -c pastor -n "__fish_pastor_using_subcommand fleet; and __fish_seen_subcommand_from update" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand fleet; and __fish_seen_subcommand_from update" -l dry-run -d 'Look at every machine and say what would be updated; change nothing'
complete -c pastor -n "__fish_pastor_using_subcommand fleet; and __fish_seen_subcommand_from update" -l json -d 'Print as a JSON object: version, ok, and per machine its status, the version it ran, its live tasks and why'
complete -c pastor -n "__fish_pastor_using_subcommand fleet; and __fish_seen_subcommand_from update" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand fleet; and __fish_seen_subcommand_from help" -f -a "update" -d 'Put pastor <version> from the head\'s artifact cache on every machine the head reaches, the head last'
complete -c pastor -n "__fish_pastor_using_subcommand fleet; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list describe try help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list describe try help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list describe try help" -f -a "install" -d 'Install a connector from GitHub: owner/repo, or owner/repo/subdir'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list describe try help" -f -a "link" -d 'Use a connector from a local directory, in place (for developing one)'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list describe try help" -f -a "uninstall" -d 'Remove an installed connector (its .env and state are kept)'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list describe try help" -f -a "unlink" -d 'Remove a linked connector; the directory itself is left alone'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list describe try help" -f -a "list" -d 'List connectors: version, connector, hooks, missing secrets'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list describe try help" -f -a "describe" -d 'One connector in full: manifest, origin, commands, config, secrets, jobs'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list describe try help" -f -a "try" -d 'Try a connector: run its command once for a job and print its items; creates no tasks and saves no cursor. `watch` runs its [watch] command instead and prints its lines'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list describe try help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from install" -l ref -d 'Branch, tag or commit to check out' -r
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from install" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from install" -l yes -d 'Do not ask for confirmation'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from install" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from link" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from link" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from uninstall" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from uninstall" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from unlink" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from unlink" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from list" -s w -l wide -d 'Add a DESCRIPTION column, cut to the terminal\'s width'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON array'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from describe" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from describe" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from describe" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from try" -l job -d 'The job whose [connector] config to use; need not exist yet' -r
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from try" -l since -d 'How far back `since` points (default: the job\'s backfill, or 0s)' -r
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from try" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from try" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from help" -f -a "install" -d 'Install a connector from GitHub: owner/repo, or owner/repo/subdir'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from help" -f -a "link" -d 'Use a connector from a local directory, in place (for developing one)'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from help" -f -a "uninstall" -d 'Remove an installed connector (its .env and state are kept)'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from help" -f -a "unlink" -d 'Remove a linked connector; the directory itself is left alone'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from help" -f -a "list" -d 'List connectors: version, connector, hooks, missing secrets'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from help" -f -a "describe" -d 'One connector in full: manifest, origin, commands, config, secrets, jobs'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from help" -f -a "try" -d 'Try a connector: run its command once for a job and print its items; creates no tasks and saves no cursor. `watch` runs its [watch] command instead and prints its lines'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and not __fish_seen_subcommand_from add list show remove apply adopt help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and not __fish_seen_subcommand_from add list show remove apply adopt help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and not __fish_seen_subcommand_from add list show remove apply adopt help" -f -a "add" -d 'Add a bundle from a directory, or from a git ref: validate it, refuse secrets, and keep it as a new version when its content changed'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and not __fish_seen_subcommand_from add list show remove apply adopt help" -f -a "list" -d 'List bundles: version, content hash, versions kept, the flocks naming each'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and not __fish_seen_subcommand_from add list show remove apply adopt help" -f -a "show" -d 'One bundle in full: what it carries, who names it, the versions kept'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and not __fish_seen_subcommand_from add list show remove apply adopt help" -f -a "remove" -d 'Remove a bundle and its versions; refused while flock.toml or a job names it'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and not __fish_seen_subcommand_from add list show remove apply adopt help" -f -a "apply" -d 'Place a machine\'s bundles in its agents\' homes: skills, hooks, bin files and instruction blocks, and take out what it no longer gets'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and not __fish_seen_subcommand_from add list show remove apply adopt help" -f -a "adopt" -d 'Take over the homes of a machine\'s agents in one step: back up what it touches, make linked files and dirs plain copies, and apply its bundles with the bundle\'s value winning over the user\'s'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and not __fish_seen_subcommand_from add list show remove apply adopt help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from add" -l git -d 'A git repository to fetch the bundle from, with --ref' -r
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from add" -l ref -d 'The branch, tag or commit to fetch; the commit it names is recorded' -r
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from add" -l path -d 'The bundle\'s directory inside the repository' -r
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from add" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from add" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from add" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON array'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from show" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from show" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from show" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from remove" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from remove" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from remove" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from apply" -l local -d 'Apply the rendered set in this directory to this machine; what the head runs on the machine' -r -F
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from apply" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from apply" -l dry-run -d 'Say what would change, and change nothing'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from apply" -l json -d 'Print as a JSON array, one object per machine'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from apply" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from adopt" -l agent -d 'Only the home of this agent; unset, every home on the machine' -r
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from adopt" -l local -d 'Adopt with the rendered set in this directory on this machine; what the head runs on the machine' -r -F
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from adopt" -l home -d 'With --local: a home to adopt, as the plan names it; unset, every home' -r
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from adopt" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from adopt" -l dry-run -d 'List what would be replaced, and change nothing'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from adopt" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from adopt" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from help" -f -a "add" -d 'Add a bundle from a directory, or from a git ref: validate it, refuse secrets, and keep it as a new version when its content changed'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from help" -f -a "list" -d 'List bundles: version, content hash, versions kept, the flocks naming each'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from help" -f -a "show" -d 'One bundle in full: what it carries, who names it, the versions kept'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from help" -f -a "remove" -d 'Remove a bundle and its versions; refused while flock.toml or a job names it'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from help" -f -a "apply" -d 'Place a machine\'s bundles in its agents\' homes: skills, hooks, bin files and instruction blocks, and take out what it no longer gets'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from help" -f -a "adopt" -d 'Take over the homes of a machine\'s agents in one step: back up what it touches, make linked files and dirs plain copies, and apply its bundles with the bundle\'s value winning over the user\'s'
complete -c pastor -n "__fish_pastor_using_subcommand bundle; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and not __fish_seen_subcommand_from place remove check list apply help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and not __fish_seen_subcommand_from place remove check list apply help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and not __fish_seen_subcommand_from place remove check list apply help" -f -a "place" -d 'Place a set of files in an agent\'s working directory before it starts, and take out what an earlier version of the set placed that this one no longer has'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and not __fish_seen_subcommand_from place remove check list apply help" -f -a "remove" -d 'Take out what pastor placed in a working directory, as long as it still holds what pastor wrote'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and not __fish_seen_subcommand_from place remove check list apply help" -f -a "check" -d 'Say whether a directory still holds the sets the head placed in it: each read on the machine and placed again as a dry run, with its state; ok only when nothing would change'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and not __fish_seen_subcommand_from place remove check list apply help" -f -a "list" -d 'The directories the head placed sets in, from its record'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and not __fish_seen_subcommand_from place remove check list apply help" -f -a "apply" -d 'Carry out a plan the head sent: what `place` and `remove` run on a push machine'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and not __fish_seen_subcommand_from place remove check list apply help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from place" -l set -d 'The set: a JSON or TOML file, a directory with a pastor-workdir.toml, or - for stdin' -r
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from place" -l bundle -d 'A stored workdir bundle in place of a set: the head builds its set for --kind from the newest version kept, with the vars of the machine\'s flock that names it' -r
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from place" -l kind -d 'With --bundle: the kind of agent that starts in the directory' -r
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from place" -l machine -d 'With --local: this machine\'s name in the flock, for a set that names a profile: settled as for a task on it, so `unrestricted` goes only where the machine\'s own profile is. Left out, the rules that look at the machine are not checked. It also names the machine in the report sent to a remote head; left out there, `[shepherd] machine` of pastor.toml, else the hostname' -r
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from place" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from place" -l local -d 'Place it in a directory on this machine, without the head'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from place" -l dry-run -d 'Say what would change, and change nothing'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from place" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from place" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from remove" -l id -d 'Only this set; unset, every set placed there' -r
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from remove" -l machine -d 'With --local: this machine\'s name in the flock, for the report sent to a remote head. Left out, `[shepherd] machine` of pastor.toml, else the hostname' -r
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from remove" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from remove" -l local -d 'Remove it from a directory on this machine, without the head'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from remove" -l dry-run -d 'Say what would change, and change nothing'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from remove" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from remove" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from check" -l id -d 'Only this set; unset, every set the head placed there' -r
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from check" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from check" -l json -d 'Print as a JSON array, one object per set'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from check" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON array'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from apply" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from apply" -l local -d 'It runs on this machine, where the head sent the plan'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from apply" -l dry-run -d 'Say what would change, and change nothing'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from apply" -l json -d 'Print as a JSON object, the answer the head reads'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from apply" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from help" -f -a "place" -d 'Place a set of files in an agent\'s working directory before it starts, and take out what an earlier version of the set placed that this one no longer has'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from help" -f -a "remove" -d 'Take out what pastor placed in a working directory, as long as it still holds what pastor wrote'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from help" -f -a "check" -d 'Say whether a directory still holds the sets the head placed in it: each read on the machine and placed again as a dry run, with its state; ok only when nothing would change'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from help" -f -a "list" -d 'The directories the head placed sets in, from its record'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from help" -f -a "apply" -d 'Carry out a plan the head sent: what `place` and `remove` run on a push machine'
complete -c pastor -n "__fish_pastor_using_subcommand workdir; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and not __fish_seen_subcommand_from list describe render help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand profile; and not __fish_seen_subcommand_from list describe render help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and not __fish_seen_subcommand_from list describe render help" -f -a "list" -d 'Every permission profile: name, where it comes from, what it extends'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and not __fish_seen_subcommand_from list describe render help" -f -a "describe" -d 'One profile with its extends followed: the allow and deny lists it adds up to'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and not __fish_seen_subcommand_from list describe render help" -f -a "render" -d 'What a profile becomes for claude or agy: its flags, env and settings, and what it cannot carry'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and not __fish_seen_subcommand_from list describe render help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON array'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from describe" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from describe" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from describe" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from render" -l kind -d 'The kind of agent to render for' -r -f -a "claude\t''
agy\t''
opencode\t''
codex\t''"
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from render" -l machine -d 'Settle the profile as for a task on this machine: `unrestricted` only where the machine\'s own profile is, and no name means the machine\'s own' -r
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from render" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from render" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from render" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from help" -f -a "list" -d 'Every permission profile: name, where it comes from, what it extends'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from help" -f -a "describe" -d 'One profile with its extends followed: the allow and deny lists it adds up to'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from help" -f -a "render" -d 'What a profile becomes for claude or agy: its flags, env and settings, and what it cannot carry'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand limit; and not __fish_seen_subcommand_from list clear help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand limit; and not __fish_seen_subcommand_from list clear help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand limit; and not __fish_seen_subcommand_from list clear help" -f -a "list" -d 'Every exhausted account and signed-out agent: its model, when it is tried again, what ran out, who saw it'
complete -c pastor -n "__fish_pastor_using_subcommand limit; and not __fish_seen_subcommand_from list clear help" -f -a "clear" -d 'Forget an account\'s limit or signed-out mark, so queued and waiting tasks start on it on the next pass'
complete -c pastor -n "__fish_pastor_using_subcommand limit; and not __fish_seen_subcommand_from list clear help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand limit; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand limit; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON array'
complete -c pastor -n "__fish_pastor_using_subcommand limit; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand limit; and __fish_seen_subcommand_from clear" -l model -d 'Clear only this model\'s limit, not the whole account\'s' -r
complete -c pastor -n "__fish_pastor_using_subcommand limit; and __fish_seen_subcommand_from clear" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand limit; and __fish_seen_subcommand_from clear" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand limit; and __fish_seen_subcommand_from help" -f -a "list" -d 'Every exhausted account and signed-out agent: its model, when it is tried again, what ran out, who saw it'
complete -c pastor -n "__fish_pastor_using_subcommand limit; and __fish_seen_subcommand_from help" -f -a "clear" -d 'Forget an account\'s limit or signed-out mark, so queued and waiting tasks start on it on the next pass'
complete -c pastor -n "__fish_pastor_using_subcommand limit; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand agent; and not __fish_seen_subcommand_from signed-in help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand agent; and not __fish_seen_subcommand_from signed-in help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand agent; and not __fish_seen_subcommand_from signed-in help" -f -a "signed-in" -d 'Run an agent\'s login check on a machine; when it passes, clear the agent\'s signed-out mark there'
complete -c pastor -n "__fish_pastor_using_subcommand agent; and not __fish_seen_subcommand_from signed-in help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand agent; and __fish_seen_subcommand_from signed-in" -l machine -d 'The machine to check it on' -r
complete -c pastor -n "__fish_pastor_using_subcommand agent; and __fish_seen_subcommand_from signed-in" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand agent; and __fish_seen_subcommand_from signed-in" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "signed-in" -d 'Run an agent\'s login check on a machine; when it passes, clear the agent\'s signed-out mark there'
complete -c pastor -n "__fish_pastor_using_subcommand agent; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand login; and not __fish_seen_subcommand_from list code cancel help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand login; and not __fish_seen_subcommand_from list code cancel help" -l json -d 'Print as a JSON object: `pending`, the login started (machine, agent, pane, url, code, paste, started; null when the agent was signed in already), and `cleared`, the marks that cleared'
complete -c pastor -n "__fish_pastor_using_subcommand login; and not __fish_seen_subcommand_from list code cancel help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand login; and not __fish_seen_subcommand_from list code cancel help" -a "list" -d 'Every login the head is watching: machine, agent, the URL and code to sign in with'
complete -c pastor -n "__fish_pastor_using_subcommand login; and not __fish_seen_subcommand_from list code cancel help" -a "code" -d 'Type the code the browser gave back into a pending login\'s pane, then Enter'
complete -c pastor -n "__fish_pastor_using_subcommand login; and not __fish_seen_subcommand_from list code cancel help" -a "cancel" -d 'Close a pending login\'s pane; the agent stays signed out'
complete -c pastor -n "__fish_pastor_using_subcommand login; and not __fish_seen_subcommand_from list code cancel help" -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand login; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand login; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON array'
complete -c pastor -n "__fish_pastor_using_subcommand login; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand login; and __fish_seen_subcommand_from code" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand login; and __fish_seen_subcommand_from code" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand login; and __fish_seen_subcommand_from cancel" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand login; and __fish_seen_subcommand_from cancel" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand login; and __fish_seen_subcommand_from help" -f -a "list" -d 'Every login the head is watching: machine, agent, the URL and code to sign in with'
complete -c pastor -n "__fish_pastor_using_subcommand login; and __fish_seen_subcommand_from help" -f -a "code" -d 'Type the code the browser gave back into a pending login\'s pane, then Enter'
complete -c pastor -n "__fish_pastor_using_subcommand login; and __fish_seen_subcommand_from help" -f -a "cancel" -d 'Close a pending login\'s pane; the agent stays signed out'
complete -c pastor -n "__fish_pastor_using_subcommand login; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and not __fish_seen_subcommand_from list add remove help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand trust; and not __fish_seen_subcommand_from list add remove help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and not __fish_seen_subcommand_from list add remove help" -f -a "list" -d 'Every saved trust: machine, repo, and when it was saved'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and not __fish_seen_subcommand_from list add remove help" -f -a "add" -d 'Save a trust, so the repo\'s tasks on that machine are answered'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and not __fish_seen_subcommand_from list add remove help" -f -a "remove" -d 'Forget a saved trust; the repo\'s next task asks again'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and not __fish_seen_subcommand_from list add remove help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON array'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from add" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from add" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from remove" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from remove" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from help" -f -a "list" -d 'Every saved trust: machine, repo, and when it was saved'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from help" -f -a "add" -d 'Save a trust, so the repo\'s tasks on that machine are answered'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from help" -f -a "remove" -d 'Forget a saved trust; the repo\'s next task asks again'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand join; and not __fish_seen_subcommand_from create list revoke help" -l name -d 'This machine\'s name in the head\'s flock.toml (the hostname when unset)' -r
complete -c pastor -n "__fish_pastor_using_subcommand join; and not __fish_seen_subcommand_from create list revoke help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand join; and not __fish_seen_subcommand_from create list revoke help" -l token-stdin -d 'Read the token from stdin; else from PASTOR_JOIN_TOKEN, else it is asked for on the terminal'
complete -c pastor -n "__fish_pastor_using_subcommand join; and not __fish_seen_subcommand_from create list revoke help" -l skip-bootstrap -d 'Join and name the serve only, for a machine set up beforehand: no herdr is fetched and no service installed'
complete -c pastor -n "__fish_pastor_using_subcommand join; and not __fish_seen_subcommand_from create list revoke help" -l json -d 'Print as a JSON object: the bootstrap report'
complete -c pastor -n "__fish_pastor_using_subcommand join; and not __fish_seen_subcommand_from create list revoke help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand join; and not __fish_seen_subcommand_from create list revoke help" -f -a "create" -d 'Make a one-time token a new machine joins the fleet with, as a pull machine; prints the token'
complete -c pastor -n "__fish_pastor_using_subcommand join; and not __fish_seen_subcommand_from create list revoke help" -f -a "list" -d 'The join tokens the head knows: id, flock, tags, what it replaces, expiry, when used'
complete -c pastor -n "__fish_pastor_using_subcommand join; and not __fish_seen_subcommand_from create list revoke help" -f -a "revoke" -d 'Remove a join token\'s key from authorized_keys and forget it'
complete -c pastor -n "__fish_pastor_using_subcommand join; and not __fish_seen_subcommand_from create list revoke help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from create" -l flock -d 'The flock the machine joins (default: the default flock)' -r
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from create" -l tag -d 'A tag the machine gets; repeat for more' -r
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from create" -l replace -d 'The one machine name already in flock.toml the token may take over, and the only name it joins as' -r
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from create" -l ttl -d 'How long the token works, like 30m or 1h' -r
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from create" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from create" -l json -d 'Print as a JSON object: the id, the token and its expiry'
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from create" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON array'
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from revoke" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from revoke" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from help" -f -a "create" -d 'Make a one-time token a new machine joins the fleet with, as a pull machine; prints the token'
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from help" -f -a "list" -d 'The join tokens the head knows: id, flock, tags, what it replaces, expiry, when used'
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from help" -f -a "revoke" -d 'Remove a join token\'s key from authorized_keys and forget it'
complete -c pastor -n "__fish_pastor_using_subcommand join; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand bridge" -l machine -d 'The machine this bridge serves (needs --agent or --shepherd), as flock.toml names it' -r
complete -c pastor -n "__fish_pastor_using_subcommand bridge" -l gen -d 'The enrollment of --machine this shepherd\'s key belongs to; every request is refused once the head moves past it (needs --shepherd)' -r
complete -c pastor -n "__fish_pastor_using_subcommand bridge" -l join -d 'Take one join request for join token ID, and nothing else: the way in for a machine that joins with it' -r
complete -c pastor -n "__fish_pastor_using_subcommand bridge" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand bridge" -l agent -d 'Pass on only what an agent on --machine may ask: its machine\'s tasks'
complete -c pastor -n "__fish_pastor_using_subcommand bridge" -l shepherd -d 'Pass on only what the shepherd of pull machine --machine may ask: its own claims, reports, trust, jobs and events'
complete -c pastor -n "__fish_pastor_using_subcommand bridge" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand inbox; and not __fish_seen_subcommand_from send help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand inbox; and not __fish_seen_subcommand_from send help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand inbox; and not __fish_seen_subcommand_from send help" -f -a "send" -d 'Send the text on stdin to a Claude session\'s inbox socket and print one JSON line: the result (accepted, held, denied, expired, refused, dropped or no-session), a detail and the message id; exits 0 only for accepted'
complete -c pastor -n "__fish_pastor_using_subcommand inbox; and not __fish_seen_subcommand_from send help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand inbox; and __fish_seen_subcommand_from send" -l config-dir -d 'Claude\'s config directory on this machine; default $CLAUDE_CONFIG_DIR, else ~/.claude' -r -F
complete -c pastor -n "__fish_pastor_using_subcommand inbox; and __fish_seen_subcommand_from send" -l session -d 'The session\'s id, as `sessionId` in its registry record' -r
complete -c pastor -n "__fish_pastor_using_subcommand inbox; and __fish_seen_subcommand_from send" -l mode -d 'The permission class the session was launched with; a bypassing session holds every message that does not say bypass' -r -f -a "prompting\t''
bypass\t''"
complete -c pastor -n "__fish_pastor_using_subcommand inbox; and __fish_seen_subcommand_from send" -l wait -d 'Seconds to listen for a receipt; Claude sends none for an accepted message, so this is also how long an accepted send takes' -r
complete -c pastor -n "__fish_pastor_using_subcommand inbox; and __fish_seen_subcommand_from send" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand inbox; and __fish_seen_subcommand_from send" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand inbox; and __fish_seen_subcommand_from help" -f -a "send" -d 'Send the text on stdin to a Claude session\'s inbox socket and print one JSON line: the result (accepted, held, denied, expired, refused, dropped or no-session), a detail and the message id; exits 0 only for accepted'
complete -c pastor -n "__fish_pastor_using_subcommand inbox; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand head; and not __fish_seen_subcommand_from set show unset help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand head; and not __fish_seen_subcommand_from set show unset help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand head; and not __fish_seen_subcommand_from set show unset help" -f -a "set" -d 'Use the head on another machine, reached over ssh; checked with one ping first'
complete -c pastor -n "__fish_pastor_using_subcommand head; and not __fish_seen_subcommand_from set show unset help" -f -a "show" -d 'Print the head this CLI uses'
complete -c pastor -n "__fish_pastor_using_subcommand head; and not __fish_seen_subcommand_from set show unset help" -f -a "unset" -d 'Use the head on this machine again'
complete -c pastor -n "__fish_pastor_using_subcommand head; and not __fish_seen_subcommand_from set show unset help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand head; and __fish_seen_subcommand_from set" -l pastor -d 'pastor\'s path on the head, if it is not on the PATH of a non-interactive shell there' -r
complete -c pastor -n "__fish_pastor_using_subcommand head; and __fish_seen_subcommand_from set" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand head; and __fish_seen_subcommand_from set" -l force -d 'Save it even if the head does not answer'
complete -c pastor -n "__fish_pastor_using_subcommand head; and __fish_seen_subcommand_from set" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand head; and __fish_seen_subcommand_from show" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand head; and __fish_seen_subcommand_from show" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand head; and __fish_seen_subcommand_from show" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand head; and __fish_seen_subcommand_from unset" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand head; and __fish_seen_subcommand_from unset" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand head; and __fish_seen_subcommand_from help" -f -a "set" -d 'Use the head on another machine, reached over ssh; checked with one ping first'
complete -c pastor -n "__fish_pastor_using_subcommand head; and __fish_seen_subcommand_from help" -f -a "show" -d 'Print the head this CLI uses'
complete -c pastor -n "__fish_pastor_using_subcommand head; and __fish_seen_subcommand_from help" -f -a "unset" -d 'Use the head on this machine again'
complete -c pastor -n "__fish_pastor_using_subcommand head; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand leave" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand leave" -l dry-run -d 'Change nothing: list what would go'
complete -c pastor -n "__fish_pastor_using_subcommand leave" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "serve" -d 'Run the daemon in the background: scheduler, machine channels, dispatch. With a head set on another machine, run headless: this machine\'s jobs and hooks, and the head\'s tasks for it as a pull machine'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "task" -d 'Manage tasks'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "queue" -d 'The queued tasks in the order they will start, and why each waits; move one'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "status" -d 'The fleet on one screen: the head, machines up and down, live tasks by state, failing jobs, exhausted accounts'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "machine" -d 'Manage the machines and which flock each is in'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "flock" -d 'Manage the flocks: named groups of machines that tasks and jobs target'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "tick" -d 'Run one scheduler pass now and report what it did'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "job" -d 'Manage jobs (files in ~/.config/pastor/jobs/). With a head elsewhere, a job whose file is here is this machine\'s; the rest are the head\'s'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "orchestrator" -d 'Manage orchestrators (files in ~/.config/pastor/orchestrators/): the head runs a pre script on a schedule and starts an agent with the orchestrator role for what it prints, or keeps one agent running through set hours'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "config" -d 'pastor.toml: the head\'s settings and the task defaults'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "completions" -d 'Print a shell completion script (fish, bash, zsh, ...) to stdout'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "events" -d 'Show the events log (task, job and machine events)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "watch" -d 'Print one line per change to act on: tasks, failing jobs, the head, connector lines'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "setup" -d 'Install pastor or herdr as a user service (systemd, or launchd on macOS)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "bootstrap" -d 'Set this machine up for a flock, as the login user: keys, client.toml, services, harnesses'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "fleet" -d 'The fleet as a whole: pastor updates'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "connector" -d 'Install, link, list and try out connectors'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "bundle" -d 'Bundles of skills, settings, MCP servers and instructions for agents\' homes'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "workdir" -d 'Sets of files placed in an agent\'s working directory before it starts'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "profile" -d 'Permission profiles: named tool allow and deny lists, built in and from pastor.toml'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "limit" -d 'The accounts that ran out of usage, which no new task starts on until they reset'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "agent" -d 'Agents on machines: check that one is signed in'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "login" -d 'Sign an agent in on a machine: open its login in a pane there, show the URL and code, and clear its signed-out mark once it passes'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "trust" -d 'The repos whose folder-trust prompt pastor answers on each machine'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "join" -d 'Join a head with a one-time token as a pull machine, or make and list the tokens on the head'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "bridge" -d 'Pass request lines from stdin to this machine\'s head; for a remote CLI over ssh'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "inbox" -d 'Talk to a Claude session\'s inbox socket on this machine'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "head" -d 'Which head this CLI uses: this machine\'s, or one on another machine over ssh'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "leave" -d 'Take from this host what pastor put there for itself: its marked authorized_keys lines and its own key files in the state directory, nothing else'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from serve" -f -a "stop" -d 'Stop the pastor serve running here (SIGTERM); agents keep running'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from serve" -f -a "status" -d 'Whether pastor serve runs here: head or headless, pid, version, service, log'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "run" -d 'Create a one-off task and dispatch it'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "list" -d 'List live tasks across the flock; --all adds finished ones'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "describe" -d 'One task in full: state, machine, agent, the models it moved through on usage limits, prompt, error, summary'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "read" -d 'Read recent output from a task\'s pane'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "attach" -d 'Attach to a task\'s agent terminal (ctrl+b q detaches); a closed Claude task\'s session reopens in a new pane'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "retry" -d 'Re-dispatch a failed or stale task as a new task (retry_of points back)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "priority" -d 'Put a queued task at another level: low, normal, high or critical'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "close" -d 'Close tasks\' panes (and with --remove-worktree their worktrees), or orphaned agents'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "prune" -d 'Delete old finished tasks; their items stay seen'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "sweep" -d 'List the worktrees pastor kept for tasks that ended, and with --yes remove those with nothing in them to lose'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "send" -d 'Type text or press keys in a live task\'s agent, to answer what it is waiting on'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "done" -d 'Mark a task done, its pane to close after close_done_after; an agent may end its own'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from queue" -f -a "move" -d 'Put a queued task elsewhere in the queue; it takes the level of where it lands'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "add" -d 'Add a machine to flock.toml, reached over ssh, locally or by a command'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "bootstrap" -d 'Set an ssh machine up from the head, as its login user, or finish a setup that stopped; pins its host key on first use and never replaces it'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "remove" -d 'Remove a machine from flock.toml; tasks already on it keep their rows'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "move" -d 'Take a machine out of every flock and put it in this one; tasks already on it stay there'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "cordon" -d 'Take no new task on a machine until it is uncordoned; the tasks it runs go on'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "uncordon" -d 'Let a cordoned machine take tasks again, and place what waited for it'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "list" -d 'A line about the head, then each machine: host, flock, channel, herdr, agents'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "describe" -d 'One machine in full: host, flock, channel, versions, agents, recent errors'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "check" -d 'Run a machine\'s provisioner\'s checks there and say what it is missing for the agents that can land on it; changes nothing, and exits 1 if a check failed'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "install" -d 'Install or update each harness the machine\'s flock pins, and herdr when pastor.toml pins it, to the pinned version, as the machine\'s user; exits 1 if one could not be'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "open" -d 'Open the full herdr UI on a machine'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "authorized-key" -d 'Print the authorized_keys line that lets a machine\'s agents reach this head; with --write, put it there on its marked line'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "list" -d 'Every flock: default or not, its machines, live agents, queued tasks'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "add" -d 'Declare a flock; with --default, new tasks and jobs go to it'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "join" -d 'Put a machine in a flock, or change its number there; it stays in its other flocks'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "leave" -d 'Take a machine out of a flock; out of its last one it is in the default flock'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "remove" -d 'Remove a flock; refused while it has machines or queued tasks, or is the default'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "default" -d 'The flock that new tasks and jobs go to'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "edit" -d 'Open flock.toml in $VISUAL or $EDITOR; save it only once it is valid'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "describe" -d 'One flock in full: default or not, its agent, machines, live tasks'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "list" -d 'Every job file: schedule, enabled, last run, next run, last result. With a head elsewhere, the head\'s jobs, then this machine\'s'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "enable" -d 'Enable a job file'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "disable" -d 'Disable a job file'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "run" -d 'Fire a job now, ignoring its schedule and `enabled`; it starts once a run already going has finished'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "reload" -d 'Re-read the job files, flock.toml and pastor.toml now instead of at the next tick'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "edit" -d 'Open a job file in $VISUAL or $EDITOR; save it only once it is valid'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "describe" -d 'One job in full: schedule, connector, dispatch, last runs, recent tasks'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from orchestrator" -f -a "list" -d 'Every orchestrator file: kind, state, schedule, last and next run, its last agent'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from orchestrator" -f -a "describe" -d 'One orchestrator in full: its settings, note, last runs with the lines each pre script printed, and recent events'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from orchestrator" -f -a "run" -d 'Run a scheduled orchestrator now, ignoring its schedule and `enabled`; it starts once a run already going has finished, and is skipped while its last agent works'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from orchestrator" -f -a "start" -d 'Start a session orchestrator now, inside its hours or not, `enabled` or not; it stops at the next hours.stop'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from orchestrator" -f -a "stop" -d 'Stop a session orchestrator: its agent gets a last message and is closed after stop_grace; it does not start on its hours again before they next end'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from orchestrator" -f -a "enable" -d 'Enable an orchestrator file'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from orchestrator" -f -a "disable" -d 'Disable an orchestrator file; an agent it started keeps running'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from orchestrator" -f -a "note" -d 'Keep the orchestrator\'s handover note (4 KiB at most), which every agent it starts gets in its prompt; the last one wins, and an empty one removes it'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from config" -f -a "edit" -d 'Open pastor.toml in $VISUAL or $EDITOR; save it only once it is valid'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from setup" -f -a "systemd" -d 'Install and manage a systemd user unit: pastor.service, or herdr.service with --herdr'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from setup" -f -a "launchd" -d 'Install and manage a launchd user agent on macOS: pastor.serve, or pastor.herdr with --herdr'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from fleet" -f -a "update" -d 'Put pastor <version> from the head\'s artifact cache on every machine the head reaches, the head last'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "install" -d 'Install a connector from GitHub: owner/repo, or owner/repo/subdir'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "link" -d 'Use a connector from a local directory, in place (for developing one)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "uninstall" -d 'Remove an installed connector (its .env and state are kept)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "unlink" -d 'Remove a linked connector; the directory itself is left alone'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "list" -d 'List connectors: version, connector, hooks, missing secrets'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "describe" -d 'One connector in full: manifest, origin, commands, config, secrets, jobs'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "try" -d 'Try a connector: run its command once for a job and print its items; creates no tasks and saves no cursor. `watch` runs its [watch] command instead and prints its lines'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from bundle" -f -a "add" -d 'Add a bundle from a directory, or from a git ref: validate it, refuse secrets, and keep it as a new version when its content changed'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from bundle" -f -a "list" -d 'List bundles: version, content hash, versions kept, the flocks naming each'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from bundle" -f -a "show" -d 'One bundle in full: what it carries, who names it, the versions kept'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from bundle" -f -a "remove" -d 'Remove a bundle and its versions; refused while flock.toml or a job names it'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from bundle" -f -a "apply" -d 'Place a machine\'s bundles in its agents\' homes: skills, hooks, bin files and instruction blocks, and take out what it no longer gets'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from bundle" -f -a "adopt" -d 'Take over the homes of a machine\'s agents in one step: back up what it touches, make linked files and dirs plain copies, and apply its bundles with the bundle\'s value winning over the user\'s'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from workdir" -f -a "place" -d 'Place a set of files in an agent\'s working directory before it starts, and take out what an earlier version of the set placed that this one no longer has'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from workdir" -f -a "remove" -d 'Take out what pastor placed in a working directory, as long as it still holds what pastor wrote'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from workdir" -f -a "check" -d 'Say whether a directory still holds the sets the head placed in it: each read on the machine and placed again as a dry run, with its state; ok only when nothing would change'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from workdir" -f -a "list" -d 'The directories the head placed sets in, from its record'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from workdir" -f -a "apply" -d 'Carry out a plan the head sent: what `place` and `remove` run on a push machine'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from profile" -f -a "list" -d 'Every permission profile: name, where it comes from, what it extends'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from profile" -f -a "describe" -d 'One profile with its extends followed: the allow and deny lists it adds up to'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from profile" -f -a "render" -d 'What a profile becomes for claude or agy: its flags, env and settings, and what it cannot carry'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from limit" -f -a "list" -d 'Every exhausted account and signed-out agent: its model, when it is tried again, what ran out, who saw it'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from limit" -f -a "clear" -d 'Forget an account\'s limit or signed-out mark, so queued and waiting tasks start on it on the next pass'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from agent" -f -a "signed-in" -d 'Run an agent\'s login check on a machine; when it passes, clear the agent\'s signed-out mark there'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from login" -f -a "list" -d 'Every login the head is watching: machine, agent, the URL and code to sign in with'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from login" -f -a "code" -d 'Type the code the browser gave back into a pending login\'s pane, then Enter'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from login" -f -a "cancel" -d 'Close a pending login\'s pane; the agent stays signed out'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from trust" -f -a "list" -d 'Every saved trust: machine, repo, and when it was saved'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from trust" -f -a "add" -d 'Save a trust, so the repo\'s tasks on that machine are answered'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from trust" -f -a "remove" -d 'Forget a saved trust; the repo\'s next task asks again'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from join" -f -a "create" -d 'Make a one-time token a new machine joins the fleet with, as a pull machine; prints the token'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from join" -f -a "list" -d 'The join tokens the head knows: id, flock, tags, what it replaces, expiry, when used'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from join" -f -a "revoke" -d 'Remove a join token\'s key from authorized_keys and forget it'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from inbox" -f -a "send" -d 'Send the text on stdin to a Claude session\'s inbox socket and print one JSON line: the result (accepted, held, denied, expired, refused, dropped or no-session), a detail and the message id; exits 0 only for accepted'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from head" -f -a "set" -d 'Use the head on another machine, reached over ssh; checked with one ping first'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from head" -f -a "show" -d 'Print the head this CLI uses'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from head" -f -a "unset" -d 'Use the head on this machine again'

# Names (jobs, flocks, machines, tasks, connectors) come from pastor itself.
function __fish_pastor_names
    set -g __fish_pastor_names (pastor __complete fish -- (commandline -opc)[2..] (commandline -ct) 2>/dev/null)
end

complete -c pastor -n __fish_pastor_names -k -f -a '(printf "%s\n" $__fish_pastor_names)'
complete -c pastor -n __fish_pastor_names -l after -l before -l connector -l fallback -l flock -l job -l machine -l model -l name -l priority -l profile -l task -r -k -f -a '(printf "%s\n" $__fish_pastor_names)'
