# Load with: eval "$(dots completions bash)"
_dots_complete() {
    local native_locale=${LC_ALL-}
    local LC_ALL=C
    local line=${COMP_LINE:0:COMP_POINT} word='' quote='' ch next
    local ansi='' decoded='' control escape
    local started=0 split=0 home_prefix=0 replacement=-1 i
    local -a words
    words=()

    # Readline gives the actual text to replace as $2. Some word breaks, such
    # as @ in npm:@scope/foo, are also part of the replacement.
    if (($# >= 2)) && [[ $line == *"$2" ]]; then
        replacement=$((${#line} - ${#2}))
    fi

    # COMP_WORDS splits backend names at colons and retains shell quotes.
    # Read the prefix up to the cursor as literal words, without expansion.
    for ((i=0; i<${#line}; i++)); do
        if ((i == replacement)); then split=${#word}; fi
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
                        '$')
                            if [[ ${line:i+1:1} == "'" ]]; then
                                ansi=''
                                ((i+=2))
                                while ((i < ${#line})); do
                                    ch=${line:i:1}
                                    [[ $ch == "'" ]] && break
                                    next=${line:i+1:1}
                                    if [[ $ch == \\ && $next == c && -n ${line:i+2:1} && ${line:i+2:1} != "'" ]]; then
                                        if [[ ${line:i+2:1} == '?' ]]; then
                                            # Bash 3.2 and newer versions decode this differently.
                                            ansi+=$'\c?'
                                        else
                                            printf -v control '%d' "'${line:i+2:1}"
                                            printf -v escape '\\%03o' "$((control & 31))"
                                            ansi+=$escape
                                        fi
                                        ((i+=2))
                                    else
                                        ansi+=$ch
                                        if [[ $ch == \\ && -n $next ]]; then
                                            ansi+=$next
                                            ((i+=1))
                                        fi
                                    fi
                                    ((i+=1))
                                done
                                # Quote percent signs before using printf's
                                # escape processing. Never evaluate shell code.
                                decoded=''
                                LC_ALL=$native_locale printf -v decoded -- "${ansi//%/%%}" 2>/dev/null || :
                                word+=$decoded
                                started=1
                                continue
                            fi ;;
                        "'"|'"')
                            quote=$ch
                            started=1
                            continue ;;
                        ' '|$'\t')
                            if ((started)); then
                                if ((home_prefix)); then word=$HOME${word:1}; fi
                                words+=( "$word" )
                            fi
                            word='' started=0 split=0 home_prefix=0
                            continue ;;
                        [\;\|\&\(\)]|$'\n')
                            words=() word='' started=0 split=0 home_prefix=0
                            continue ;;
                        '~')
                            if ((!started)) && [[ ${line:i+1:1} == / ]]; then home_prefix=1; fi ;;
                    esac
                    if ((replacement < 0)) && [[ $COMP_WORDBREAKS == *"$ch"* ]]; then
                        split=$((${#word} + 1))
                    fi
                fi ;;
        esac
        word+=$ch
        started=1
    done
    if ((replacement == ${#line})); then split=${#word}; fi

    COMPREPLY=()
    local reply value native_paths=0 file_prefix='' literal_home=0 file_word=$word nospace=0
    reply=$(command dots __complete bash "${words[@]:1}" "$word" 2>/dev/null) || return
    case $reply in
        :files|:dirs|:files=|:dirs=)
            native_paths=1
            if [[ $reply == *= ]]; then file_prefix=${word%%=*}=; file_word=${word#*=}; fi
            if [[ $file_word == '~/'* ]] && { ((!home_prefix)) || [[ -n $file_prefix ]]; }; then
                literal_home=1
                file_word=./$file_word
            fi
            if [[ $reply == :dirs* ]]; then reply=$(compgen -d -- "$file_word")
            else reply=$(compgen -f -- "$file_word"); fi ;;
    esac
    while IFS= read -r value; do
        if ((!native_paths)); then
            [[ $value == *$'\t'nospace ]] && nospace=1
            value=${value%%$'\t'*}
        elif ((literal_home)); then
            value=${value#./}
        fi
        [[ -n $value ]] || continue
        value=$file_prefix$value
        if ((split)) && [[ $value != "${word:0:split}"* ]]; then continue; fi
        COMPREPLY+=( "${value:split}" )
    done <<< "$reply"
    if (( BASH_VERSINFO[0] >= 4 && ${#COMPREPLY[@]} == 1 && !nospace )) && [[ ${COMPREPLY[0]} != */ ]]; then
        compopt +o nospace
    fi
}

# Bash 3.2 has no compopt. A fixed nospace keeps prefixes such as brew-cask:
# open for typing, and filenames lets Readline handle quoting and directories.
complete -o filenames -o nospace -F _dots_complete dots
