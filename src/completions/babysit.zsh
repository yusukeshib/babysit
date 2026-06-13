#compdef babysit

# Complete known session ids (directories under ~/.babysit/sessions) plus the
# `latest` selector. Read straight from disk so completion stays fast and
# never has to spawn babysit.
__babysit_sessions() {
    local -a sessions
    local __bs_dir="$HOME/.babysit/sessions"
    if [[ -d "$__bs_dir" ]]; then
        for sess in "$__bs_dir"/*(N/); do
            sessions+=("${sess:t}")
        done
    fi
    sessions+=("latest")
    _describe 'session' sessions
}

_babysit() {
    local curcontext="$curcontext" state line
    typeset -A opt_args

    _arguments -C \
        '1: :->subcmd' \
        '*:: :->args'

    case $state in
        subcmd)
            local -a subcmds
            subcmds=(
                'run:Wrap a shell command in a PTY'
                'list:List all babysit sessions'
                'ls:List all babysit sessions'
                'status:Show status of a session'
                'st:Show status of a session'
                'info:Show status of a session'
                'log:Show recent output from the wrapped command'
                'logs:Show recent output from the wrapped command'
                'restart:Restart the wrapped command'
                'r:Restart the wrapped command'
                'kill:Terminate the wrapped command'
                'stop:Terminate the wrapped command'
                'send:Send text to the wrapped command stdin'
                'type:Send text to the wrapped command stdin'
                'wait:Block until the command exits and return its code'
                'attach:Attach your terminal to a session (detach: Ctrl-\ Ctrl-\)'
                'a:Attach your terminal to a session (detach: Ctrl-\ Ctrl-\)'
                'detach:Detach any terminal attached to a session'
                'prune:Delete finished or dead sessions'
                'upgrade:Self-update to the latest version'
                'config:Output shell integration (eval "$(babysit config zsh)")'
            )
            _describe 'subcommand' subcmds
            ;;
        args)
            case ${words[1]} in
                run)
                    _arguments \
                        '--id=[Session id to assign]:id:' \
                        '(-d --detach)'{-d,--detach}'[Run detached in the background]' \
                        '--no-tty[Use pipes instead of a PTY (clean line output)]' \
                        '--timeout=[Auto-terminate after e.g. 30s, 10m, 2h]:duration:' \
                        '(-)1:command:_command_names -e' \
                        '*::arguments:_normal'
                    ;;
                list|ls)
                    _arguments '--json[Output as JSON]'
                    ;;
                status|st|info)
                    _arguments \
                        '(-s --session)'{-s,--session}'[Session id]:session:__babysit_sessions' \
                        '--json[Output as JSON]'
                    ;;
                log|logs)
                    _arguments \
                        '(-s --session)'{-s,--session}'[Session id]:session:__babysit_sessions' \
                        '--tail=[Show only the last N lines]:lines:' \
                        '--raw[Include raw ANSI escapes]' \
                        '--since=[Only output bytes after this raw-log offset]:bytes:' \
                        '(-f --follow)'{-f,--follow}'[Stream new output live until exit]' \
                        '--json[Emit JSON {text, offset, done}]'
                    ;;
                restart|r|kill|stop|send|type|attach|a|detach)
                    _arguments \
                        '(-s --session)'{-s,--session}'[Session id]:session:__babysit_sessions'
                    ;;
                wait)
                    _arguments \
                        '(-s --session)'{-s,--session}'[Session id]:session:__babysit_sessions' \
                        '--timeout=[Give up after e.g. 30s, 10m]:duration:'
                    ;;
                prune)
                    _arguments '--dry-run[Print what would be deleted, but do not delete]'
                    ;;
                config)
                    if (( CURRENT == 2 )); then
                        local -a shells
                        # shellcheck disable=SC2034  # used by _describe below
                        shells=('zsh:Zsh integration' 'bash:Bash integration')
                        _describe 'shell' shells
                    fi
                    ;;
            esac
            ;;
    esac
}

compdef _babysit babysit
