# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_pastor_global_optspecs
    string join \n skill h/help V/version
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

complete -c pastor -n "__fish_pastor_needs_command" -l skill -d 'Print the agent skill (SKILL.md) for this version and exit'
complete -c pastor -n "__fish_pastor_needs_command" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_needs_command" -s V -l version -d 'Print version'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "serve" -d 'Run the daemon: scheduler, machine channels, dispatch'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "task" -d 'Manage tasks'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "machine" -d 'Manage the machines and which flock each is in'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "flock" -d 'Manage the flocks: named groups of machines that tasks and jobs target'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "open" -d 'Open the full herdr UI on a machine'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "tick" -d 'Run one scheduler pass now and report what it did'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "job" -d 'Manage jobs (files in ~/.config/pastor/jobs/)'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "completions" -d 'Print a shell completion script (fish, bash, zsh, ...) to stdout'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "events" -d 'Show the events log (task, job and machine events)'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "setup" -d 'Install pastor or herdr as a systemd user service'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "connector" -d 'Install, link, list and try out connectors'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "trust" -d 'The repos whose folder-trust prompt pastor answers on each machine'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand serve" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach retry close prune send help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach retry close prune send help" -f -a "run" -d 'Create a one-off task and dispatch it'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach retry close prune send help" -f -a "list" -d 'List live tasks across the flock; --all adds finished ones'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach retry close prune send help" -f -a "show" -d 'Show one task row'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach retry close prune send help" -f -a "read" -d 'Read recent output from a task\'s pane'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach retry close prune send help" -f -a "attach" -d 'Attach to a task\'s agent terminal (ctrl+b q detaches)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach retry close prune send help" -f -a "retry" -d 'Re-dispatch a failed or stale task as a new task (retry_of points back)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach retry close prune send help" -f -a "close" -d 'Close a task\'s pane (and with --remove-worktree its worktree), or an orphaned agent'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach retry close prune send help" -f -a "prune" -d 'Delete old finished tasks; their items stay seen'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach retry close prune send help" -f -a "send" -d 'Type text or press keys in a live task\'s agent, to answer what it is waiting on'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach retry close prune send help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l prompt-file -d 'Read the prompt from this file on this machine (\'-\' for stdin); it spares long prompts the shell\'s quoting' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l repo -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l flock -d 'Only this flock\'s machines take the task (default: the flock of --machine, else the default flock)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l machine -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l agent -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l agent-arg -d 'One argument for the agent; repeat it, in order, for more. Replaces the flock\'s and `[defaults]` agent_args. The next word is always the value, dashes and all' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l branch -d 'Branch for the worktree (needs --worktree; a plain workspace has no branch)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l tag -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l timeout -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l worktree -d 'A git worktree per task, branched from --repo (so it needs --repo)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l json
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l job -d 'Only tasks from this job (omit for one-off `run` tasks)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l flock -d 'Only tasks of this flock' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l machine -d 'Only tasks on this machine' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l blocked -d 'Only blocked tasks, needing a human'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l done -d 'Only done tasks'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l all -d 'Every task, finished ones too (done, failed, stale, closed)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l json -d 'Print full task records as JSON instead of a table'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from show" -l json
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from show" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from read" -l lines -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from read" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from attach" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from retry" -l json
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from retry" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from close" -l remove-worktree -d 'Remove the task\'s worktree too (refused if it has uncommitted changes)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from close" -l json
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from close" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l older-than -d 'Only tasks that finished longer ago than this (30m, 12h, 3d)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l done -d 'Prune done tasks'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l failed -d 'Prune failed tasks'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l closed -d 'Prune closed tasks'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -l json
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from prune" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -l key -d 'A named key to press after the text (Enter, Down, esc, ctrl+c); repeat for more, in order' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -l no-enter -d 'Type the text without pressing Enter after it'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -l trust -d 'Accept the agent\'s folder-trust prompt with its trust keys, and trust the task\'s repo on its machine from now on'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -l json
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from send" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "run" -d 'Create a one-off task and dispatch it'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "list" -d 'List live tasks across the flock; --all adds finished ones'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "show" -d 'Show one task row'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "read" -d 'Read recent output from a task\'s pane'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "attach" -d 'Attach to a task\'s agent terminal (ctrl+b q detaches)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "retry" -d 'Re-dispatch a failed or stale task as a new task (retry_of points back)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "close" -d 'Close a task\'s pane (and with --remove-worktree its worktree), or an orphaned agent'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "prune" -d 'Delete old finished tasks; their items stay seen'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "send" -d 'Type text or press keys in a live task\'s agent, to answer what it is waiting on'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list help" -f -a "add"
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list help" -f -a "remove"
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list help" -f -a "move" -d 'Put a machine in another flock; tasks already on it stay there'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list help" -f -a "list" -d 'A line about the head, then each machine: host, flock, channel, herdr, agents'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove move list help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l command -d 'Developer option: the bridge command as one string, split on whitespace (`--command "fake-herdr --connect /tmp/h.sock"`). Words containing spaces go in flock.toml by hand' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l session -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l max-agents -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l tag -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l flock -d 'The flock it joins (default: the default flock)' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l local
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l herdr -d 'Also save it in herdr\'s sidebar (runs `herdr machine add`)'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from remove" -l herdr -d 'Also remove herdr\'s saved machine with this label'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from remove" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from move" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from list" -l flock -d 'Only the machines of this flock' -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from list" -l json
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "add"
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "remove"
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "move" -d 'Put a machine in another flock; tasks already on it stay there'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "list" -d 'A line about the head, then each machine: host, flock, channel, herdr, agents'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add remove default help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add remove default help" -f -a "list" -d 'Every flock: default or not, its machines, live agents, queued tasks'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add remove default help" -f -a "add" -d 'Declare a flock; with --default, new tasks and jobs go to it'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add remove default help" -f -a "remove" -d 'Remove a flock; refused while it has machines or queued tasks, or is the default'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add remove default help" -f -a "default" -d 'Make another flock the default; machines stay in their flocks'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from list add remove default help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from list" -l json
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -l default
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from remove" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from default" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "list" -d 'Every flock: default or not, its machines, live agents, queued tasks'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "add" -d 'Declare a flock; with --default, new tasks and jobs go to it'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "remove" -d 'Remove a flock; refused while it has machines or queued tasks, or is the default'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "default" -d 'Make another flock the default; machines stay in their flocks'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand open" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand tick" -l job -d 'Only this job, and run it whether or not it is due' -r
complete -c pastor -n "__fish_pastor_using_subcommand tick" -l dry-run -d 'Run connectors and show what would be created; write nothing'
complete -c pastor -n "__fish_pastor_using_subcommand tick" -l json
complete -c pastor -n "__fish_pastor_using_subcommand tick" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload help" -f -a "list" -d 'Every job file: schedule, enabled, last run, next run, last result'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload help" -f -a "enable" -d 'Enable a job file'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload help" -f -a "disable" -d 'Disable a job file'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload help" -f -a "run" -d 'Fire a job now, ignoring its schedule, the overlap rule and `enabled`'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload help" -f -a "reload" -d 'Re-read the job files now instead of at the next tick'
complete -c pastor -n "__fish_pastor_using_subcommand job; and not __fish_seen_subcommand_from list enable disable run reload help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from list" -l json
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from enable" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from disable" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from run" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from reload" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from help" -f -a "list" -d 'Every job file: schedule, enabled, last run, next run, last result'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from help" -f -a "enable" -d 'Enable a job file'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from help" -f -a "disable" -d 'Disable a job file'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from help" -f -a "run" -d 'Fire a job now, ignoring its schedule, the overlap rule and `enabled`'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from help" -f -a "reload" -d 'Re-read the job files now instead of at the next tick'
complete -c pastor -n "__fish_pastor_using_subcommand job; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand completions" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand events" -l task -d 'Only events about this task (t-N or N)' -r
complete -c pastor -n "__fish_pastor_using_subcommand events" -l follow -d 'Keep printing new events as they are written (reads the file; works with the daemon down)'
complete -c pastor -n "__fish_pastor_using_subcommand events" -l json -d 'One JSON record per line, the same shape hooks get on stdin'
complete -c pastor -n "__fish_pastor_using_subcommand events" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and not __fish_seen_subcommand_from systemd help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and not __fish_seen_subcommand_from systemd help" -f -a "systemd" -d 'Install and manage a systemd user unit: pastor.service, or herdr.service with --herdr'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and not __fish_seen_subcommand_from systemd help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -l herdr -d 'Install herdr.service (the herdr server) instead, for a flock machine'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -l enable -d 'Enable the unit at login'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -l start -d 'Start the unit now'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -l now -d 'With --enable, start the unit now too (`systemctl --user enable --now`)'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -l stop -d 'Stop the unit now'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -s y -l yes -d 'Skip the confirmation prompt; needed when stdin is not a terminal (a script, a task, `ssh host pastor setup systemd`)'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from help" -f -a "systemd" -d 'Install and manage a systemd user unit: pastor.service, or herdr.service with --herdr'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list run help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list run help" -f -a "install" -d 'Install a connector from GitHub: owner/repo, or owner/repo/subdir'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list run help" -f -a "link" -d 'Use a connector from a local directory, in place (for developing one)'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list run help" -f -a "uninstall" -d 'Remove an installed connector (its .env and state are kept)'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list run help" -f -a "unlink" -d 'Remove a linked connector; the directory itself is left alone'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list run help" -f -a "list" -d 'List connectors: version, connector, hooks, missing secrets'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list run help" -f -a "run" -d 'Run a connector\'s command once for a job and print its items; creates no tasks and saves no cursor'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and not __fish_seen_subcommand_from install link uninstall unlink list run help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from install" -l ref -d 'Branch, tag or commit to check out' -r
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from install" -l yes -d 'Do not ask for confirmation'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from install" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from link" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from uninstall" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from unlink" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from list" -l json
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from run" -l job -d 'The job whose [connector] config to use; need not exist yet' -r
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from run" -l since -d 'How far back `since` points (default: the job\'s backfill, or 0s)' -r
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from run" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from help" -f -a "install" -d 'Install a connector from GitHub: owner/repo, or owner/repo/subdir'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from help" -f -a "link" -d 'Use a connector from a local directory, in place (for developing one)'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from help" -f -a "uninstall" -d 'Remove an installed connector (its .env and state are kept)'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from help" -f -a "unlink" -d 'Remove a linked connector; the directory itself is left alone'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from help" -f -a "list" -d 'List connectors: version, connector, hooks, missing secrets'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from help" -f -a "run" -d 'Run a connector\'s command once for a job and print its items; creates no tasks and saves no cursor'
complete -c pastor -n "__fish_pastor_using_subcommand connector; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and not __fish_seen_subcommand_from list remove help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and not __fish_seen_subcommand_from list remove help" -f -a "list" -d 'Every saved trust: machine, repo, and when it was saved'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and not __fish_seen_subcommand_from list remove help" -f -a "remove" -d 'Forget a saved trust; the repo\'s next task asks again'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and not __fish_seen_subcommand_from list remove help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from list" -l json
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from remove" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from help" -f -a "list" -d 'Every saved trust: machine, repo, and when it was saved'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from help" -f -a "remove" -d 'Forget a saved trust; the repo\'s next task asks again'
complete -c pastor -n "__fish_pastor_using_subcommand trust; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine flock open tick job completions events setup connector trust help" -f -a "serve" -d 'Run the daemon: scheduler, machine channels, dispatch'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine flock open tick job completions events setup connector trust help" -f -a "task" -d 'Manage tasks'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine flock open tick job completions events setup connector trust help" -f -a "machine" -d 'Manage the machines and which flock each is in'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine flock open tick job completions events setup connector trust help" -f -a "flock" -d 'Manage the flocks: named groups of machines that tasks and jobs target'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine flock open tick job completions events setup connector trust help" -f -a "open" -d 'Open the full herdr UI on a machine'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine flock open tick job completions events setup connector trust help" -f -a "tick" -d 'Run one scheduler pass now and report what it did'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine flock open tick job completions events setup connector trust help" -f -a "job" -d 'Manage jobs (files in ~/.config/pastor/jobs/)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine flock open tick job completions events setup connector trust help" -f -a "completions" -d 'Print a shell completion script (fish, bash, zsh, ...) to stdout'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine flock open tick job completions events setup connector trust help" -f -a "events" -d 'Show the events log (task, job and machine events)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine flock open tick job completions events setup connector trust help" -f -a "setup" -d 'Install pastor or herdr as a systemd user service'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine flock open tick job completions events setup connector trust help" -f -a "connector" -d 'Install, link, list and try out connectors'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine flock open tick job completions events setup connector trust help" -f -a "trust" -d 'The repos whose folder-trust prompt pastor answers on each machine'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine flock open tick job completions events setup connector trust help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "run" -d 'Create a one-off task and dispatch it'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "list" -d 'List live tasks across the flock; --all adds finished ones'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "show" -d 'Show one task row'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "read" -d 'Read recent output from a task\'s pane'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "attach" -d 'Attach to a task\'s agent terminal (ctrl+b q detaches)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "retry" -d 'Re-dispatch a failed or stale task as a new task (retry_of points back)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "close" -d 'Close a task\'s pane (and with --remove-worktree its worktree), or an orphaned agent'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "prune" -d 'Delete old finished tasks; their items stay seen'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "send" -d 'Type text or press keys in a live task\'s agent, to answer what it is waiting on'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "add"
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "remove"
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "move" -d 'Put a machine in another flock; tasks already on it stay there'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "list" -d 'A line about the head, then each machine: host, flock, channel, herdr, agents'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "list" -d 'Every flock: default or not, its machines, live agents, queued tasks'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "add" -d 'Declare a flock; with --default, new tasks and jobs go to it'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "remove" -d 'Remove a flock; refused while it has machines or queued tasks, or is the default'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "default" -d 'Make another flock the default; machines stay in their flocks'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "list" -d 'Every job file: schedule, enabled, last run, next run, last result'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "enable" -d 'Enable a job file'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "disable" -d 'Disable a job file'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "run" -d 'Fire a job now, ignoring its schedule, the overlap rule and `enabled`'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "reload" -d 'Re-read the job files now instead of at the next tick'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from setup" -f -a "systemd" -d 'Install and manage a systemd user unit: pastor.service, or herdr.service with --herdr'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "install" -d 'Install a connector from GitHub: owner/repo, or owner/repo/subdir'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "link" -d 'Use a connector from a local directory, in place (for developing one)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "uninstall" -d 'Remove an installed connector (its .env and state are kept)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "unlink" -d 'Remove a linked connector; the directory itself is left alone'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "list" -d 'List connectors: version, connector, hooks, missing secrets'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from connector" -f -a "run" -d 'Run a connector\'s command once for a job and print its items; creates no tasks and saves no cursor'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from trust" -f -a "list" -d 'Every saved trust: machine, repo, and when it was saved'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from trust" -f -a "remove" -d 'Forget a saved trust; the repo\'s next task asks again'
