# Bash completion for babysit. Source via: eval "$(babysit config bash)"

# Echo known session ids (directories under ~/.babysit/sessions). Read
# straight from disk so completion never spawns babysit.
__babysit_sessions() {
	local dir="$HOME/.babysit/sessions" out=""
	if [[ -d "$dir" ]]; then
		local sess
		for sess in "$dir"/*/; do
			[[ -d "$sess" ]] && out+=" $(basename "$sess")"
		done
	fi
	printf '%s' "$out"
}

_babysit() {
	local cur prev words cword
	_init_completion || return

	local subcommands="run list status log screenshot send key expect wait-idle wait resize flag unflag restart kill attach detach prune upgrade config help"

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
		-*) COMPREPLY=($(compgen -W "--id --detach --no-tty --timeout --idle-timeout --size --json" -- "$cur")) ;;
		*) COMPREPLY=($(compgen -c -- "$cur")) ;;
		esac
		;;
	wait)
		[[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--session --timeout" -- "$cur"))
		;;
	wait-idle)
		[[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--session --settle --timeout" -- "$cur"))
		;;
	expect)
		[[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--session --timeout --since --from-now --raw --screen --json" -- "$cur"))
		;;
	key | resize | flag | unflag)
		[[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--session --json" -- "$cur"))
		;;
	list | ls)
		[[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--json" -- "$cur"))
		;;
	status | st | info)
		[[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--session --json" -- "$cur"))
		;;
	log | logs)
		[[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--session --tail --grep --raw --since --follow --json" -- "$cur"))
		;;
	screenshot | shot)
		case "$prev" in
		--format)
			COMPREPLY=($(compgen -W "plain ansi json" -- "$cur"))
			return
			;;
		esac
		[[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--session --format --trim" -- "$cur"))
		;;
	send | type)
		[[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--session --no-newline --json" -- "$cur"))
		;;
	restart | r | kill | stop | detach)
		[[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--session --json" -- "$cur"))
		;;
	attach | a)
		[[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--session" -- "$cur"))
		;;
	prune)
		[[ "$cur" == -* ]] && COMPREPLY=($(compgen -W "--dry-run --json" -- "$cur"))
		;;
	config)
		[[ $cword -eq 2 ]] && COMPREPLY=($(compgen -W "zsh bash" -- "$cur"))
		;;
	help)
		# help takes a subcommand name as its argument.
		[[ $cword -eq 2 ]] && COMPREPLY=($(compgen -W "run list status log screenshot send key expect wait-idle wait resize flag unflag restart kill attach detach prune upgrade config" -- "$cur"))
		;;
	esac
}

complete -F _babysit babysit
