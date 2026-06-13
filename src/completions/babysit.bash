# Bash completion for babysit. Source via: eval "$(babysit config bash)"

# Echo known session ids (directories under ~/.babysit/sessions) plus the
# `latest` selector. Read straight from disk so completion never spawns
# babysit.
__babysit_sessions() {
    local dir="$HOME/.babysit/sessions" out=""
    if [[ -d "$dir" ]]; then
        local sess
        for sess in "$dir"/*/; do
            [[ -d "$sess" ]] && out+=" $(basename "$sess")"
        done
    fi
    printf '%s latest' "$out"
}

_babysit() {
    local cur prev words cword
    _init_completion || return

    local subcommands="run list ls status st info log logs restart r kill stop send type prune upgrade config help"

    if [[ $cword -eq 1 ]]; then
        COMPREPLY=($(compgen -W "$subcommands" -- "$cur"))
        return
    fi

    # `-s <id>` / `--session <id>` completes session ids for any subcommand.
    if [[ "$prev" == "-s" || "$prev" == "--session" ]]; then
        COMPREPLY=($(compgen -W "$(__babysit_sessions)" -- "$cur"))
        return
    fi

    case "${words[1]}" in
        run)
            case "$cur" in
                -*) COMPREPLY=($(compgen -W "--id -d --detach" -- "$cur")) ;;
                *)  COMPREPLY=($(compgen -c -- "$cur")) ;;
            esac
            ;;
        list|ls)
            [[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--json" -- "$cur"))
            ;;
        status|st|info)
            [[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "-s --session --json" -- "$cur"))
            ;;
        log|logs)
            [[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "-s --session --tail --raw" -- "$cur"))
            ;;
        restart|r|kill|stop|send|type)
            [[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "-s --session" -- "$cur"))
            ;;
        prune)
            [[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--dry-run" -- "$cur"))
            ;;
        config)
            [[ $cword -eq 2 ]] && COMPREPLY=($(compgen -W "zsh bash" -- "$cur"))
            ;;
    esac
}

complete -F _babysit babysit
