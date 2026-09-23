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
complete -c pastor -n "__fish_pastor_needs_command" -f -a "run" -d 'Create a one-off task and dispatch it'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "list" -d 'List tasks across the flock'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "task" -d 'Inspect a task'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "flock" -d 'Manage machines'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "attach" -d 'Attach to a task\'s agent terminal (ctrl+b q detaches)'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "open" -d 'Open the full herdr UI on a machine'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "completions" -d 'Print a shell completion script (fish, bash, zsh, ...) to stdout'
complete -c pastor -n "__fish_pastor_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand serve" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand run" -l repo -r
complete -c pastor -n "__fish_pastor_using_subcommand run" -l machine -r
complete -c pastor -n "__fish_pastor_using_subcommand run" -l agent -r
complete -c pastor -n "__fish_pastor_using_subcommand run" -l branch -r
complete -c pastor -n "__fish_pastor_using_subcommand run" -l tag -r
complete -c pastor -n "__fish_pastor_using_subcommand run" -l timeout -r
complete -c pastor -n "__fish_pastor_using_subcommand run" -l worktree
complete -c pastor -n "__fish_pastor_using_subcommand run" -l json
complete -c pastor -n "__fish_pastor_using_subcommand run" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand list" -l job -d 'Only tasks from this job (omit for one-off `run` tasks)' -r
complete -c pastor -n "__fish_pastor_using_subcommand list" -l machine -d 'Only tasks on this machine' -r
complete -c pastor -n "__fish_pastor_using_subcommand list" -l blocked -d 'Only blocked tasks, needing a human'
complete -c pastor -n "__fish_pastor_using_subcommand list" -l done -d 'Only done tasks'
complete -c pastor -n "__fish_pastor_using_subcommand list" -l all -d 'Include closed tasks'
complete -c pastor -n "__fish_pastor_using_subcommand list" -l json -d 'Print full task records as JSON instead of a table'
complete -c pastor -n "__fish_pastor_using_subcommand list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from show read help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from show read help" -f -a "show"
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from show read help" -f -a "read"
complete -c pastor -n "__fish_pastor_using_subcommand task; and not __fish_seen_subcommand_from show read help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from show" -l json
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from show" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from read" -l lines -r
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from read" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "show"
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "read"
complete -c pastor -n "__fish_pastor_using_subcommand task; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from add remove list status help" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from add remove list status help" -f -a "add"
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from add remove list status help" -f -a "remove"
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from add remove list status help" -f -a "list"
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from add remove list status help" -f -a "status"
complete -c pastor -n "__fish_pastor_using_subcommand flock; and not __fish_seen_subcommand_from add remove list status help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -l command -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -l session -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -l max-agents -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -l tag -r
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -l local
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from add" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from remove" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from list" -l json
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from status" -l json
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from status" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "add"
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "remove"
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "list"
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "status"
complete -c pastor -n "__fish_pastor_using_subcommand flock; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand attach" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand open" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand completions" -s h -l help -d 'Print help'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve run list task flock attach open completions help" -f -a "serve" -d 'Run the daemon: scheduler, machine channels, dispatch'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve run list task flock attach open completions help" -f -a "run" -d 'Create a one-off task and dispatch it'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve run list task flock attach open completions help" -f -a "list" -d 'List tasks across the flock'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve run list task flock attach open completions help" -f -a "task" -d 'Inspect a task'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve run list task flock attach open completions help" -f -a "flock" -d 'Manage machines'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve run list task flock attach open completions help" -f -a "attach" -d 'Attach to a task\'s agent terminal (ctrl+b q detaches)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve run list task flock attach open completions help" -f -a "open" -d 'Open the full herdr UI on a machine'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve run list task flock attach open completions help" -f -a "completions" -d 'Print a shell completion script (fish, bash, zsh, ...) to stdout'
complete -c pastor -n "__fish_pastor_using_subcommand help; and not __fish_seen_subcommand_from serve run list task flock attach open completions help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "show"
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from task" -f -a "read"
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "add"
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "remove"
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "list"
complete -c pastor -n "__fish_pastor_using_subcommand help; and __fish_seen_subcommand_from flock" -f -a "status"
