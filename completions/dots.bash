# Load with: eval "$(dots completions bash)"
_dots_complete() {
    local LC_ALL=C
    local line=${COMP_LINE:0:COMP_POINT} word='' quote='' ch next
    local started=0 split=0 i
    local -a words
    words=()

    # COMP_WORDS splits backend names at colons and retains shell quotes.
    # Read the prefix up to the cursor as literal words, without expansion.
    for ((i=0; i<${#line}; i++)); do
        ch=${line:i:1}
        case $ch in
            "$quote")
                quote=''
                continue ;;
            \\)
                next=${line:i+1:1}
                if [[ $quote != "'" && -n $next ]]; then
                    if [[ -z $quote || $next == [\`\$\"\\$'\n'] ]]; then
                        ((i+=1))
                        [[ $next == $'\n' ]] && continue
                        ch=$next
                    fi
                fi ;;
            *)
                if [[ -z $quote ]]; then
                    case $ch in
                        "'"|'"')
                            quote=$ch
                            started=1
                            continue ;;
                        ' '|$'\t')
                            if ((started)); then words+=( "$word" ); fi
                            word='' started=0 split=0
                            continue ;;
                        [\;\|\&\(\)]|$'\n')
                            words=() word='' started=0 split=0
                            continue ;;
                    esac
                    if [[ $COMP_WORDBREAKS == *"$ch"* ]]; then
                        split=$((${#word} + 1))
                    fi
                fi ;;
        esac
        word+=$ch
        started=1
    done

    COMPREPLY=()
    local reply value file_prefix='' file_word=$word
    reply=$(MISE_SAFE=1 MISE_NO_ENV=1 MISE_NO_HOOKS=1 MISE_OFFLINE=1 MISE_AUTO_INSTALL=0 MISE_EXEC_AUTO_INSTALL=0 MISE_AUTO_UPDATE=0 command dots internal shell-complete "${words[@]:1}" "$word" 2>/dev/null) || return
    case $reply in
        :files|:dirs)
            if [[ $word == -*=* ]]; then file_prefix=${word%%=*}=; file_word=${word#*=}; fi
            if [[ $reply == :dirs ]]; then reply=$(compgen -d -- "$file_word")
            else reply=$(compgen -f -- "$file_word"); fi ;;
    esac
    while IFS= read -r value; do
        value=${value%%$'\t'*}
        [[ -n $value ]] || continue
        value=$file_prefix$value
        if ((split)) && [[ $value != "${word:0:split}"* ]]; then continue; fi
        COMPREPLY+=( "${value:split}" )
    done <<< "$reply"
    if (( BASH_VERSINFO[0] >= 4 && ${#COMPREPLY[@]} == 1 )) && [[ ${COMPREPLY[0]} != *: && ${COMPREPLY[0]} != */ ]]; then
        compopt +o nospace
    fi
}

# Bash 3.2 has no compopt. A fixed nospace keeps prefixes such as brew-cask:
# open for typing, and filenames lets Readline handle quoting and directories.
complete -o filenames -o nospace -F _dots_complete dots
