# Load with: dots completions fish | source
function __dots_query
    set -l prior (commandline -opc)
    set -l token (commandline -ct)
    set -l current (string unescape -- "$token")
    # The tokenizer strips quotes before we can tell which earlier tildes
    # expand. Keep the raw prefix for that distinction, without expanding it.
    set -l source (commandline -pc | string collect)

    command dots __complete fish "$argv[1]" "$source" $prior[2..-1] "$current" 2>/dev/null
end

function __dots_plain_file_token
    set -l current (string unescape -- "$argv[1]")
    not string match -q -- '-*' "$current"
end

function __dots_native_file_slot
    # Fish's dynamic candidates use a tab as the description separator.
    # Let its own file completer handle ordinary tokens, including tabs.
    set -l prior (commandline -opc)
    string match -q -- '--*' "$prior[-1]"; or return 1
    __dots_plain_file_token (commandline -ct); or return 1
    set -l reply (__dots_query path-slot)
    test "$reply" = :files
end

function __dots_complete
    set -l token (commandline -ct)
    set -l reply (__dots_query values)
    set -q reply[1]; or return
    switch "$reply"
        case :files :dirs :files= :dirs=
            if test "$reply" = :files; and __dots_plain_file_token "$token"
                return
            end
            set -l prefix ''
            set -l current (string unescape -- "$token")
            set -l search
            if string match -q '*=' -- "$reply"
                set -l parts (string split -m 1 = -- "$current")
                set prefix "$parts[1]="
                set current "$parts[2]"
            else if string match -q '~/*' -- "$token"
                set search '~/'(string escape -- (string replace -r '^~/' '' -- "$current"))
            end
            # The path helpers ask Fish to complete a nested command line.
            # Quote the filename so only a leading unquoted ~/ can expand.
            if not set -q search[1]
                set search (string escape -- "$current")
            end
            set -l option_like 0
            if string match -q -- '-*' "$current"
                set option_like 1
                set search "./$search"
            end
            set -l matches
            if string match -q ':dirs*' -- "$reply"
                set matches (__fish_complete_directories "$search")
            else
                set matches (__fish_complete_path "$search")
            end
            if test "$option_like" = 1
                set matches (string replace -r '^\./' '' -- $matches)
            end
            printf '%s\n' "$prefix"$matches
        case '*'
            string replace -r '\tnospace$' '' -- $reply
    end
end

complete -c dots -f -a '(__dots_complete)'
complete -c dots -F -n __dots_native_file_slot
