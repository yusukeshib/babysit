#compdef babysit

# Return the host selected anywhere on the current command line. `words` is a
# zsh completion-special array and is dynamically visible from helper functions.
__babysit_host() {
    local i
    for (( i = 1; i <= ${#words}; i++ )); do
        case "${words[$i]}" in
            --host)
                if (( i < ${#words} )); then
                    print -r -- "${words[$(( i + 1 ))]}"
                    return
                fi
                ;;
            --host=*)
                print -r -- "${words[$i]#--host=}"
                return
                ;;
        esac
    done
    print -r -- local
}

# Complete known session ids. Local completion reads ~/.babysit directly so it
# stays fast; a selected remote host is queried through babysit's SSH routing.
#
# Each candidate carries a description mirroring `babysit ls`. This shows up as
# a second column in plain zsh completion and in the fzf-tab list/preview.
__babysit_sessions() {
    local -a sessions
    local __bs_host="$(__babysit_host)"
    local __bs_dir="$HOME/.babysit/sessions"
    if [[ "$__bs_host" != local ]]; then
        local line id desc
        while IFS= read -r line; do
            [[ -z "$line" || "$line" == ID[[:space:]]* || "$line" == \(no\ sessions\) ]] && continue
            # Last-output preview lines are indented and are not sessions.
            [[ "$line" == [[:space:]]* ]] && continue
            id="${line%%[[:space:]]*}"
            desc="${line#$id}"
            sessions+=("${id}:${desc}")
        done < <(command babysit --host "$__bs_host" list 2>/dev/null)
    elif [[ -d "$__bs_dir" ]]; then
        local sess id state code cmd flag desc
        for sess in "$__bs_dir"/*(N/); do
            id="${sess:t}"
            # State (and exit code) from status.json. exited+code -> exit:N,
            # mirroring the label `babysit ls` prints.
            state=""
            if [[ -r "$sess/status.json" ]]; then
                state=$(sed -n 's/.*"state"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$sess/status.json")
                if [[ "$state" == exited ]]; then
                    code=$(sed -n 's/.*"exit_code"[[:space:]]*:[[:space:]]*\(-\{0,1\}[0-9][0-9]*\).*/\1/p' "$sess/status.json")
                    [[ -n "$code" ]] && state="exit:$code"
                fi
            fi
            # Command from meta.json: join the quoted entries of the "cmd" array.
            cmd=""
            if [[ -r "$sess/meta.json" ]]; then
                cmd=$(sed -n '/"cmd"[[:space:]]*:[[:space:]]*\[/,/]/p' "$sess/meta.json" \
                    | sed -n 's/^[[:space:]]*"\(.*\)",\{0,1\}[[:space:]]*$/\1/p' \
                    | tr '\n' ' ')
                cmd="${cmd%% }"
            fi
            # A flagged session (babysit flag) is prefixed with ⚑.
            flag=""
            [[ -e "$sess/note" ]] && flag="⚑ "
            desc="${state:+$state  }$flag$cmd"
            sessions+=("${id}:${desc}")
        done
    fi
    _describe -V 'session' sessions
}

_babysit() {
    # state/line/opt_args are populated and read by zsh's _arguments machinery;
    # the linter can't model that, so silence its unused-variable warnings.
    # shellcheck disable=SC2034
    local curcontext="$curcontext" state line
    # shellcheck disable=SC2034
    typeset -A opt_args

    _arguments -C \
        '--host=[Run on an SSH destination]:host:_hosts' \
        '1: :->subcmd' \
        '*:: :->args'

    case $state in
        subcmd)
            # subcmds is consumed by `_describe` below.
            local -a subcmds
            # shellcheck disable=SC2034
            subcmds=(
                'run:Wrap a shell command in a PTY'
                'list:List all babysit sessions'
                'status:Show status of a session'
                'log:Show recent output from the wrapped command'
                'screenshot:Capture the current visible screen'
                'restart:Restart the wrapped command'
                'kill:Terminate the wrapped command'
                'send:Send text to the wrapped command stdin'
                'key:Send named keys (Enter, Up, Esc, C-c, F1, …)'
                'expect:Block until a regex appears in the output'
                'wait-idle:Block until output has been quiet for a while'
                'wait:Block until the command exits and return its code'
                'resize:Resize the wrapped command terminal (COLSxROWS)'
                'flag:Flag a session for human attention'
                'unflag:Clear a session attention flag'
                'attach:Attach your terminal to a session (detach: Ctrl-\ Ctrl-\)'
                'detach:Detach any terminal attached to a session'
                'prune:Delete finished or dead sessions'
                'upgrade:Self-update to the latest version'
                'config:Output shell integration (eval "$(babysit config zsh)")'
                'help:Show help for a subcommand'
            )
            _describe 'subcommand' subcmds
            ;;
        args)
            local subcmd_index=1
            if [[ ${words[1]} == --host ]]; then
                subcmd_index=3
            elif [[ ${words[1]} == --host=* ]]; then
                subcmd_index=2
            fi
            case ${words[$subcmd_index]} in
                run)
                    _arguments \
                        '--id=[Session id to assign]:id:' \
                        '--detach[Run detached in the background]' \
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
                        '--session[Session id]:session:__babysit_sessions' \
                        '--json[Output as JSON]'
                    ;;
                log|logs)
                    _arguments \
                        '--session[Session id]:session:__babysit_sessions' \
                        '--tail=[Show only the last N lines]:lines:' \
                        '--grep=[Only show lines matching this regex]:regex:' \
                        '--raw[Include raw ANSI escapes]' \
                        '--since=[Only output bytes after this raw-log offset]:bytes:' \
                        '--follow[Stream new output live until exit]' \
                        '--json[Emit JSON {text, offset, done}]'
                    ;;
                screenshot|shot)
                    _arguments \
                        '--session[Session id]:session:__babysit_sessions' \
                        '--format=[Output format]:format:(plain ansi json)' \
                        '--trim[Drop trailing blank lines and whitespace]'
                    ;;
                send|type)
                    _arguments \
                        '--session[Session id]:session:__babysit_sessions' \
                        '--no-newline[Do not append a trailing newline]' \
                        '--json[Emit JSON {sent, offset}]'
                    ;;
                restart|r|kill|stop|detach|key|resize|flag|unflag)
                    _arguments \
                        '--session[Session id]:session:__babysit_sessions' \
                        '--json[Emit a JSON result]'
                    ;;
                attach|a)
                    _arguments \
                        '--session[Session id]:session:__babysit_sessions' \
                        '--no-reconnect[Return instead of reconnecting after SSH loss]'
                    ;;
                expect)
                    _arguments \
                        '--session[Session id]:session:__babysit_sessions' \
                        '--timeout=[Give up after e.g. 30s, 2m; exits 124]:duration:' \
                        '--since=[Start scanning from this raw-log byte offset]:bytes:' \
                        '--from-now[Only match output produced from now on]' \
                        '--raw[Match against raw output incl. ANSI escapes]' \
                        '--screen[Match against the rendered TUI screen]' \
                        '--json[Emit JSON {matched, offset}]'
                    ;;
                wait-idle)
                    _arguments \
                        '--session[Session id]:session:__babysit_sessions' \
                        '--settle=[Required quiet period, e.g. 500ms, 2s]:duration:' \
                        '--timeout=[Give up after e.g. 30s; exits 124]:duration:'
                    ;;
                wait)
                    _arguments \
                        '--session[Session id]:session:__babysit_sessions' \
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
                help)
                    if (( CURRENT == 2 )); then
                        # help takes a subcommand name as its argument.
                        local -a helpcmds
                        # shellcheck disable=SC2034  # used by _describe below
                        helpcmds=(
                            run list status log screenshot send key expect
                            wait-idle wait resize flag unflag restart kill
                            attach detach prune upgrade config
                        )
                        _describe 'subcommand' helpcmds
                    fi
                    ;;
            esac
            ;;
    esac
}

compdef _babysit babysit
