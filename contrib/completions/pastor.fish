# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_pastor_global_optspecs
    string join \n h/help V/version
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

complete -c pastor -n "__fish_pastor_needs_command" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_needs_command" -s V -l version -d 'Print version'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "serve" -d 'Run the daemon: scheduler, machine channels, dispatch'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "task" -d 'Manage tasks'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "machine" -d 'Manage the machines in the flock'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "open" -d 'Open the full herdr UI on a machine'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "tick" -d 'Run one scheduler pass now and report what it did'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "job" -d 'Manage jobs (files in ~/.config/pastor/jobs/)'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "completions" -d 'Print a shell completion script (fish, bash, zsh, ...) to stdout'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "events" -d 'Show the events log (task, job and machine events)'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "setup" -d 'Install pastor or herdr as a systemd user service'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand serve" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach help" -f -a "run" -d 'Create a one-off task and dispatch it'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach help" -f -a "list" -d 'List live tasks across the flock; --all adds finished ones'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach help" -f -a "show" -d 'Show one task row'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach help" -f -a "read" -d 'Read recent output from a task\'s pane'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach help" -f -a "attach" -d 'Attach to a task\'s agent terminal (ctrl+b q detaches)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from run list show read attach help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l repo -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l machine -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l agent -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l agent-arg -d 'One argument for the agent; repeat it, in order, for more. Replaces `[defaults] agent_args`. The next word is always the value, dashes and all' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l branch -d 'Branch for the worktree (needs --worktree; a plain workspace has no branch)' -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l tag -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l timeout -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l worktree
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -l json
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from run" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from list" -l job -d 'Only tasks from this job (omit for one-off `run` tasks)' -r
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
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "run" -d 'Create a one-off task and dispatch it'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "list" -d 'List live tasks across the flock; --all adds finished ones'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "show" -d 'Show one task row'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "read" -d 'Read recent output from a task\'s pane'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "attach" -d 'Attach to a task\'s agent terminal (ctrl+b q detaches)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove list status help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove list status help" -f -a "add"
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove list status help" -f -a "remove"
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove list status help" -f -a "list"
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove list status help" -f -a "status"
complete -c pastor -n "__fish_pastor_using_subcommand machine; and not __fish_seen_subcommand_from add remove list status help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l command -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l session -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l max-agents -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l tag -r
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l local
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -l herdr -d 'Also save it in herdr\'s sidebar (runs `herdr machine add`)'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from add" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from remove" -l herdr -d 'Also remove herdr\'s saved machine with this label'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from remove" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from list" -l json
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from status" -l json
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from status" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "add"
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "remove"
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "list"
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "status"
complete -c pastor -n "__fish_pastor_using_subcommand machine; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
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
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from systemd" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from help" -f -a "systemd" -d 'Install and manage a systemd user unit: pastor.service, or herdr.service with --herdr'
complete -c pastor -n "__fish_pastor_using_subcommand setup; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine open tick job completions events setup help" -f -a "serve" -d 'Run the daemon: scheduler, machine channels, dispatch'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine open tick job completions events setup help" -f -a "task" -d 'Manage tasks'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine open tick job completions events setup help" -f -a "machine" -d 'Manage the machines in the flock'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine open tick job completions events setup help" -f -a "open" -d 'Open the full herdr UI on a machine'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine open tick job completions events setup help" -f -a "tick" -d 'Run one scheduler pass now and report what it did'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine open tick job completions events setup help" -f -a "job" -d 'Manage jobs (files in ~/.config/pastor/jobs/)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine open tick job completions events setup help" -f -a "completions" -d 'Print a shell completion script (fish, bash, zsh, ...) to stdout'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine open tick job completions events setup help" -f -a "events" -d 'Show the events log (task, job and machine events)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine open tick job completions events setup help" -f -a "setup" -d 'Install pastor or herdr as a systemd user service'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve task machine open tick job completions events setup help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "run" -d 'Create a one-off task and dispatch it'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "list" -d 'List live tasks across the flock; --all adds finished ones'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "show" -d 'Show one task row'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "read" -d 'Read recent output from a task\'s pane'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "attach" -d 'Attach to a task\'s agent terminal (ctrl+b q detaches)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "add"
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "remove"
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "list"
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from machine" -f -a "status"
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "list" -d 'Every job file: schedule, enabled, last run, next run, last result'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "enable" -d 'Enable a job file'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "disable" -d 'Disable a job file'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "run" -d 'Fire a job now, ignoring its schedule, the overlap rule and `enabled`'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from job" -f -a "reload" -d 'Re-read the job files now instead of at the next tick'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from setup" -f -a "systemd" -d 'Install and manage a systemd user unit: pastor.service, or herdr.service with --herdr'
