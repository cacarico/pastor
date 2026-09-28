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
complete -c pastor -n "__fish_pastor_needs_command" -f -a "serve" -d 'Run the daemon: scheduler, machine channels, dispatch. With a head set on another machine, run headless: only this machine\'s jobs and hooks'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "task" -d 'Manage tasks'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "queue" -d 'The queued tasks in the order they will start, and why each waits; move one'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "machine" -d 'Manage the machines and which flock each is in'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "flock" -d 'Manage the flocks: named groups of machines that tasks and jobs target'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "tick" -d 'Run one scheduler pass now and report what it did'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "job" -d 'Manage jobs (files in ~/.config/pastor/jobs/). With a head elsewhere, a job whose file is here is this machine\'s; the rest are the head\'s'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "config" -d 'pastor.toml: the head\'s settings and the task defaults'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "completions" -d 'Print a shell completion script (fish, bash, zsh, ...) to stdout'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "events" -d 'Show the events log (task, job and machine events)'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "watch" -d 'Print one line per change to act on: tasks, failing jobs, the head, connector lines'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "setup" -d 'Install pastor or herdr as a user service (systemd, or launchd on macOS)'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "connector" -d 'Install, link, list and try out connectors'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "profile" -d 'Permission profiles: named tool allow and deny lists, built in and from pastor.toml'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "trust" -d 'The repos whose folder-trust prompt pastor answers on each machine'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "bridge" -d 'Pass request lines from stdin to this machine\'s head; for a remote CLI over ssh'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "head" -d 'Which head this CLI uses: this machine\'s, or one on another machine over ssh'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand serve" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand serve" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune send done help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune send done help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune send done help" -f -a "run" -d 'Create a one-off task and dispatch it'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune send done help" -f -a "list" -d 'List live tasks across the flock; --all adds finished ones'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune send done help" -f -a "describe" -d 'One task in full: state, machine, agent, prompt, error, summary'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune send done help" -f -a "read" -d 'Read recent output from a task\'s pane'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune send done help" -f -a "attach" -d 'Attach to a task\'s agent terminal (ctrl+b q detaches); a closed Claude task\'s session reopens in a new pane'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune send done help" -f -a "retry" -d 'Re-dispatch a failed or stale task as a new task (retry_of points back)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune send done help" -f -a "priority" -d 'Put a queued task at another level: low, normal, high or critical'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune send done help" -f -a "close" -d 'Close a task\'s pane (and with --remove-worktree its worktree), or an orphaned agent'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune send done help" -f -a "prune" -d 'Delete old finished tasks; their items stay seen'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune send done help" -f -a "send" -d 'Type text or press keys in a live task\'s agent, to answer what it is waiting on'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune send done help" -f -a "done" -d 'Mark a task done, its pane to close after close_done_after; an agent may end its own'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list describe read attach retry priority close prune send done help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l prompt-file -d 'Read the prompt from this file on this machine (\'-\' for stdin); it spares long prompts the shell\'s quoting' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l repo -d 'The repo the agent works in: a path on the machine that runs the task, not on this one' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l flock -d 'Only this flock\'s machines take the task (default: the flock of --machine, else the default flock)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l machine -d 'Run it on this machine (a name from flock.toml) instead of any free one' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l agent -d 'The agent command to start, like claude or codex (default: the machine\'s, else its flock\'s, else `[defaults]`, else claude)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l agent-arg -d 'One argument for the agent; repeat it, in order, for more. Replaces the flock\'s and `[defaults]` agent_args. The next word is always the value, dashes and all' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l model -d 'Run this model, a name from `[models]` in pastor.toml; its args go before the agent\'s (default: the machine\'s, else its flock\'s, else `[defaults] model`, else none)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l priority -d 'Queue at this level: low, normal, high or critical; dispatch takes higher levels first (default: the pinned machine\'s, else its flock\'s, else `[defaults] priority`, else normal)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l profile -d 'Run under this permission profile, built in or from `[profiles]` in pastor.toml: a Claude agent gets its allow and deny lists and never asks (default: the machine\'s, else its flock\'s, else `[defaults] profile`, else none)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l branch -d 'Branch for the worktree (needs --worktree; a plain workspace has no branch)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l tag -d 'Only a machine with this tag takes the task; repeat for more, and it needs them all' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l timeout -d 'Mark the task stale once it has run this long (30m, 2h; default: `[defaults]` timeout)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l label -d 'Label template of the workspace pastor makes for the task, with {{ task.id }}, {{ flock }}, {{ machine }}, {{ job }} and {{ item.key }} (default: the flock\'s `label`, else `[defaults] label`, else {{ flock }}/{{ task.id }}). The agent stays t-N' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l place -d 'Where the agent\'s pane goes: repo (under the repo it works on), own (its own workspace), pastor (the `pastor` workspace) or pane:<workspace> (default: `[defaults] place`, else repo)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l role -d 'What the agent may change through the head: agent (read, and end its own task) or orchestrator (also run, retry, send to and close tasks and enable and disable jobs). Only a person may start an orchestrator, never a task' -r -f -a "agent\t'Reads, and `task done` for its own task; everything else is refused unless `agents_change_fleet` is on'
orchestrator\t'Also runs, retries, types into and closes tasks and enables and disables jobs (`IpcRequest::orchestrator_may`). Only a person makes one: `task run --role orchestrator` from outside any task'"
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l description -d 'One line on what the task is about, for `task list --wide` and `describe` (default: the prompt\'s first line)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l preempt -d 'A critical task only: on a full machine, pause the newest low Claude task there (its session resumes when a slot frees) and take its slot'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l worktree -d 'A git worktree per task, branched from --repo (so it needs --repo)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l job -d 'Only tasks from this job (omit for one-off `run` tasks)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l flock -d 'Only tasks of this flock' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l machine -d 'Only tasks on this machine' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l blocked -d 'Only blocked tasks, needing a human'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l done -d 'Only done tasks'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l all -d 'Every task, finished ones too (done, failed, stale, closed)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -s w -l wide -d 'Add a DESCRIPTION column, cut to the terminal\'s width'
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
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from close" -l remove-worktree -d 'Remove the task\'s worktree too (refused if it has uncommitted changes)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from close" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from close" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l older-than -d 'Only tasks that finished longer ago than this (30m, 12h, 3d)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l done -d 'Prune done tasks'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l failed -d 'Prune failed tasks'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l closed -d 'Prune closed tasks'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -l key -d 'A named key to press after the text (Enter, Down, esc, ctrl+c); repeat for more, in order' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -l no-enter -d 'Type the text without pressing Enter after it'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -l trust -d 'Accept the agent\'s folder-trust prompt with its trust keys, and trust the task\'s repo on its machine from now on'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from done" -l summary -d 'What you did, its first line the outcome: done, partial, blocked or nothing to do (kept to 2,000 characters)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from done" -l summary-file -d 'Read the summary from a file, or - for stdin' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from done" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from done" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from done" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "run" -d 'Create a one-off task and dispatch it'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "list" -d 'List live tasks across the flock; --all adds finished ones'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "describe" -d 'One task in full: state, machine, agent, prompt, error, summary'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "read" -d 'Read recent output from a task\'s pane'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "attach" -d 'Attach to a task\'s agent terminal (ctrl+b q detaches); a closed Claude task\'s session reopens in a new pane'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "retry" -d 'Re-dispatch a failed or stale task as a new task (retry_of points back)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "priority" -d 'Put a queued task at another level: low, normal, high or critical'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "close" -d 'Close a task\'s pane (and with --remove-worktree its worktree), or an orphaned agent'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "prune" -d 'Delete old finished tasks; their items stay seen'
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
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list describe open authorized-key help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list describe open authorized-key help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list describe open authorized-key help" -f -a "add" -d 'Add a machine to flock.toml, reached over ssh, locally or by a command'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list describe open authorized-key help" -f -a "remove" -d 'Remove a machine from flock.toml; tasks already on it keep their rows'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list describe open authorized-key help" -f -a "move" -d 'Put a machine in another flock; tasks already on it stay there'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list describe open authorized-key help" -f -a "list" -d 'A line about the head, then each machine: host, flock, channel, herdr, agents'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list describe open authorized-key help" -f -a "describe" -d 'One machine in full: host, flock, channel, versions, agents, recent errors'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list describe open authorized-key help" -f -a "open" -d 'Open the full herdr UI on a machine'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list describe open authorized-key help" -f -a "authorized-key" -d 'Print the authorized_keys line that lets a machine\'s agents reach this head'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list describe open authorized-key help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l command -d 'Developer option: the bridge command as one string, split on whitespace (`--command "fake-herdr --connect /tmp/h.sock"`). Words containing spaces go in flock.toml by hand' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l session -d 'The herdr session on the machine that agents run in' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l max-agents -d 'How many tasks it runs at once' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l job-slots -d 'Extra slots only tasks from jobs take, on top of --max-agents; 0 for none' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l burst -d 'How many past --max-agents a critical task may start; 0 for none' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l tag -d 'A label a task\'s --tag can ask for; repeat for more' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l flock -d 'The flock it joins (default: the default flock)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l description -d 'One line on what the machine is for, for `machine list --wide`' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l local -d 'This machine itself, through herdr\'s local socket; no ssh'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l herdr -d 'Also save it in herdr\'s sidebar (runs `herdr machine add`)'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from remove" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from remove" -l herdr -d 'Also remove herdr\'s saved machine with this label'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from remove" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from move" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from move" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from list" -l flock -d 'Only the machines of this flock' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from list" -s w -l wide -d 'Add a DESCRIPTION column, cut to the terminal\'s width'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON object, {head, machines}: the head\'s row, then the machines'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from describe" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from describe" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from describe" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from open" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from open" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from authorized-key" -l key -d 'The public key of the machine\'s user: a .pub file, or - for stdin' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from authorized-key" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from authorized-key" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "add" -d 'Add a machine to flock.toml, reached over ssh, locally or by a command'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "remove" -d 'Remove a machine from flock.toml; tasks already on it keep their rows'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "move" -d 'Put a machine in another flock; tasks already on it stay there'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "list" -d 'A line about the head, then each machine: host, flock, channel, herdr, agents'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "describe" -d 'One machine in full: host, flock, channel, versions, agents, recent errors'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "open" -d 'Open the full herdr UI on a machine'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "authorized-key" -d 'Print the authorized_keys line that lets a machine\'s agents reach this head'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add remove default edit describe help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add remove default edit describe help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add remove default edit describe help" -f -a "list" -d 'Every flock: default or not, its machines, live agents, queued tasks'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add remove default edit describe help" -f -a "add" -d 'Declare a flock; with --default, new tasks and jobs go to it'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add remove default edit describe help" -f -a "remove" -d 'Remove a flock; refused while it has machines or queued tasks, or is the default'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add remove default edit describe help" -f -a "default" -d 'The flock that new tasks and jobs go to'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add remove default edit describe help" -f -a "edit" -d 'Open flock.toml in $VISUAL or $EDITOR; save it only once it is valid'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add remove default edit describe help" -f -a "describe" -d 'One flock in full: default or not, its agent, machines, live tasks'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add remove default edit describe help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from list" -s w -l wide -d 'Add a DESCRIPTION column, cut to the terminal\'s width'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON array'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -l description -d 'One line on what the flock is for, for `flock list --wide`' -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -l default -d 'Make it the default flock too'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -s h -l help -d 'Print help'
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
complete -c pastor -n "__fish_pastor_using_subcommand config; and not __fish_seen_subcommand_from edit help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand config; and not __fish_seen_subcommand_from edit help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand config; and not __fish_seen_subcommand_from edit help" -f -a "edit" -d 'Open pastor.toml in $VISUAL or $EDITOR; save it only once it is valid'
complete -c pastor -n "__fish_pastor_using_subcommand config; and not __fish_seen_subcommand_from edit help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand config; and __fish_seen_subcommand_from edit" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
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
complete -c pastor -n "__fish_pastor_using_subcommand profile; and not __fish_seen_subcommand_from list describe help" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand profile; and not __fish_seen_subcommand_from list describe help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and not __fish_seen_subcommand_from list describe help" -f -a "list" -d 'Every permission profile: name, where it comes from, what it extends'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and not __fish_seen_subcommand_from list describe help" -f -a "describe" -d 'One profile with its extends followed: the allow and deny lists it adds up to'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and not __fish_seen_subcommand_from list describe help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from list" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from list" -l json -d 'Print as a JSON array'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from describe" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from describe" -l json -d 'Print as a JSON object'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from describe" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from help" -f -a "list" -d 'Every permission profile: name, where it comes from, what it extends'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from help" -f -a "describe" -d 'One profile with its extends followed: the allow and deny lists it adds up to'
complete -c pastor -n "__fish_pastor_using_subcommand profile; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
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
complete -c pastor -n "__fish_pastor_using_subcommand bridge" -l machine -d 'The machine whose agents this bridge serves, as flock.toml names it' -r
complete -c pastor -n "__fish_pastor_using_subcommand bridge" -l head -d 'Use the head at this ssh destination for this command (over PASTOR_HEAD and client.toml)' -r
complete -c pastor -n "__fish_pastor_using_subcommand bridge" -l agent -d 'Pass on only what an agent on --machine may ask: its machine\'s tasks'
complete -c pastor -n "__fish_pastor_using_subcommand bridge" -s h -l help -d 'Print help'
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
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "serve" -d 'Run the daemon: scheduler, machine channels, dispatch. With a head set on another machine, run headless: only this machine\'s jobs and hooks'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "task" -d 'Manage tasks'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "queue" -d 'The queued tasks in the order they will start, and why each waits; move one'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "machine" -d 'Manage the machines and which flock each is in'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "flock" -d 'Manage the flocks: named groups of machines that tasks and jobs target'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "tick" -d 'Run one scheduler pass now and report what it did'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "job" -d 'Manage jobs (files in ~/.config/pastor/jobs/). With a head elsewhere, a job whose file is here is this machine\'s; the rest are the head\'s'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "config" -d 'pastor.toml: the head\'s settings and the task defaults'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "completions" -d 'Print a shell completion script (fish, bash, zsh, ...) to stdout'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "events" -d 'Show the events log (task, job and machine events)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "watch" -d 'Print one line per change to act on: tasks, failing jobs, the head, connector lines'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "setup" -d 'Install pastor or herdr as a user service (systemd, or launchd on macOS)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "connector" -d 'Install, link, list and try out connectors'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "profile" -d 'Permission profiles: named tool allow and deny lists, built in and from pastor.toml'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "trust" -d 'The repos whose folder-trust prompt pastor answers on each machine'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "bridge" -d 'Pass request lines from stdin to this machine\'s head; for a remote CLI over ssh'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "head" -d 'Which head this CLI uses: this machine\'s, or one on another machine over ssh'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task queue machine flock tick job config completions events watch setup connector profile trust bridge head help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "run" -d 'Create a one-off task and dispatch it'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "list" -d 'List live tasks across the flock; --all adds finished ones'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "describe" -d 'One task in full: state, machine, agent, prompt, error, summary'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "read" -d 'Read recent output from a task\'s pane'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "attach" -d 'Attach to a task\'s agent terminal (ctrl+b q detaches); a closed Claude task\'s session reopens in a new pane'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "retry" -d 'Re-dispatch a failed or stale task as a new task (retry_of points back)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "priority" -d 'Put a queued task at another level: low, normal, high or critical'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "close" -d 'Close a task\'s pane (and with --remove-worktree its worktree), or an orphaned agent'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "prune" -d 'Delete old finished tasks; their items stay seen'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "send" -d 'Type text or press keys in a live task\'s agent, to answer what it is waiting on'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "done" -d 'Mark a task done, its pane to close after close_done_after; an agent may end its own'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from queue" -f -a "move" -d 'Put a queued task elsewhere in the queue; it takes the level of where it lands'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "add" -d 'Add a machine to flock.toml, reached over ssh, locally or by a command'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "remove" -d 'Remove a machine from flock.toml; tasks already on it keep their rows'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "move" -d 'Put a machine in another flock; tasks already on it stay there'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "list" -d 'A line about the head, then each machine: host, flock, channel, herdr, agents'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "describe" -d 'One machine in full: host, flock, channel, versions, agents, recent errors'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "open" -d 'Open the full herdr UI on a machine'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "authorized-key" -d 'Print the authorized_keys line that lets a machine\'s agents reach this head'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "list" -d 'Every flock: default or not, its machines, live agents, queued tasks'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "add" -d 'Declare a flock; with --default, new tasks and jobs go to it'
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
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from config" -f -a "edit" -d 'Open pastor.toml in $VISUAL or $EDITOR; save it only once it is valid'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from setup" -f -a "systemd" -d 'Install and manage a systemd user unit: pastor.service, or herdr.service with --herdr'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from setup" -f -a "launchd" -d 'Install and manage a launchd user agent on macOS: pastor.serve, or pastor.herdr with --herdr'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "install" -d 'Install a connector from GitHub: owner/repo, or owner/repo/subdir'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "link" -d 'Use a connector from a local directory, in place (for developing one)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "uninstall" -d 'Remove an installed connector (its .env and state are kept)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "unlink" -d 'Remove a linked connector; the directory itself is left alone'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "list" -d 'List connectors: version, connector, hooks, missing secrets'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "describe" -d 'One connector in full: manifest, origin, commands, config, secrets, jobs'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "try" -d 'Try a connector: run its command once for a job and print its items; creates no tasks and saves no cursor. `watch` runs its [watch] command instead and prints its lines'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from profile" -f -a "list" -d 'Every permission profile: name, where it comes from, what it extends'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from profile" -f -a "describe" -d 'One profile with its extends followed: the allow and deny lists it adds up to'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from trust" -f -a "list" -d 'Every saved trust: machine, repo, and when it was saved'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from trust" -f -a "add" -d 'Save a trust, so the repo\'s tasks on that machine are answered'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from trust" -f -a "remove" -d 'Forget a saved trust; the repo\'s next task asks again'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from head" -f -a "set" -d 'Use the head on another machine, reached over ssh; checked with one ping first'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from head" -f -a "show" -d 'Print the head this CLI uses'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from head" -f -a "unset" -d 'Use the head on this machine again'

# Names (jobs, flocks, machines, tasks, connectors) come from pastor itself.
function __fish_pastor_names
    set -g __fish_pastor_names (pastor __complete fish -- (commandline -opc)[2..] (commandline -ct) 2>/dev/null)
end

complete -c pastor -n __fish_pastor_names -k -f -a '(printf "%s\n" $__fish_pastor_names)'
complete -c pastor -n __fish_pastor_names -l after -l before -l connector -l flock -l job -l machine -l model -l priority -l profile -l task -r -k -f -a '(printf "%s\n" $__fish_pastor_names)'
