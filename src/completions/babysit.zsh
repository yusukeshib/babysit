#compdef babysit

# Complete known session ids (directories under ~/.babysit/sessions). Read
# straight from disk so completion stays fast and never has to spawn babysit.
__babysit_sessions() {
    local -a sessions
    local __bs_dir="$HOME/.babysit/sessions"
    if [[ -d "$__bs_dir" ]]; then
        for sess in "$__bs_dir"/*(N/); do
            sessions+=("${sess:t}")
        done
    fi
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
                'screenshot:Capture the current visible screen'
                'shot:Capture the current visible screen'
                'restart:Restart the wrapped command'
                'r:Restart the wrapped command'
                'kill:Terminate the wrapped command'
                'stop:Terminate the wrapped command'
                'send:Send text to the wrapped command stdin'
                'type:Send text to the wrapped command stdin'
                'key:Send named keys (Enter, Up, Esc, C-c, F1, …)'
                'expect:Block until a regex appears in the output'
                'wait-idle:Block until output has been quiet for a while'
                'wait:Block until the command exits and return its code'
                'resize:Resize the wrapped command terminal (COLSxROWS)'
                'flag:Flag a session for human attention'
                'unflag:Clear a session attention flag'
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
                        '--idle-timeout=[Auto-terminate after this long with no output]:duration:' \
                        '--size=[Initial terminal size COLSxROWS]:size:' \
                        '--json[Print the session id as JSON]' \
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
                        '--grep=[Only show lines matching this regex]:regex:' \
                        '--raw[Include raw ANSI escapes]' \
                        '--since=[Only output bytes after this raw-log offset]:bytes:' \
                        '(-f --follow)'{-f,--follow}'[Stream new output live until exit]' \
                        '--json[Emit JSON {text, offset, done}]'
                    ;;
                screenshot|shot)
                    _arguments \
                        '(-s --session)'{-s,--session}'[Session id]:session:__babysit_sessions' \
                        '--format=[Output format]:format:(plain ansi json)' \
                        '--trim[Drop trailing blank lines and whitespace]'
                    ;;
                send|type)
                    _arguments \
                        '(-s --session)'{-s,--session}'[Session id]:session:__babysit_sessions' \
                        '(-n --no-newline)'{-n,--no-newline}'[Do not append a trailing newline]' \
                        '--json[Emit JSON {sent, offset}]'
                    ;;
                restart|r|kill|stop|detach|key|resize|flag|unflag)
                    _arguments \
                        '(-s --session)'{-s,--session}'[Session id]:session:__babysit_sessions' \
                        '--json[Emit a JSON result]'
                    ;;
                attach|a)
                    _arguments \
                        '(-s --session)'{-s,--session}'[Session id]:session:__babysit_sessions'
                    ;;
                expect)
                    _arguments \
                        '(-s --session)'{-s,--session}'[Session id]:session:__babysit_sessions' \
                        '--timeout=[Give up after e.g. 30s, 2m; exits 124]:duration:' \
                        '--since=[Start scanning from this raw-log byte offset]:bytes:' \
                        '--from-now[Only match output produced from now on]' \
                        '--raw[Match against raw output incl. ANSI escapes]' \
                        '--json[Emit JSON {matched, offset}]'
                    ;;
                wait-idle)
                    _arguments \
                        '(-s --session)'{-s,--session}'[Session id]:session:__babysit_sessions' \
                        '--settle=[Required quiet period, e.g. 500ms, 2s]:duration:' \
                        '--timeout=[Give up after e.g. 30s; exits 124]:duration:'
                    ;;
                wait)
                    _arguments \
                        '(-s --session)'{-s,--session}'[Session id]:session:__babysit_sessions' \
                        '--timeout=[Give up after e.g. 30s, 10m]:duration:'
                    ;;
                prune)
                    _arguments \
                        '--dry-run[Print what would be deleted, but do not delete]' \
                        '--json[Emit the deleted/would-delete sessions as JSON]'
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
