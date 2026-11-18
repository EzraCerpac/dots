# Load with: dots completions fish | source
function __dots_complete
    set -l prior (commandline -opc)
    set -l token (commandline -ct)
    set -l current (string unescape -- "$token")

    set -l reply (MISE_SAFE=1 MISE_NO_ENV=1 MISE_NO_HOOKS=1 MISE_OFFLINE=1 MISE_AUTO_INSTALL=0 MISE_EXEC_AUTO_INSTALL=0 MISE_AUTO_UPDATE=0 command dots internal shell-complete $prior[2..-1] "$current" 2>/dev/null)
    set -q reply[1]; or return
    switch "$reply"
        case :files :dirs :files= :dirs=
            set -l prefix ''
            if string match -q '*=' -- "$reply"
                set -l parts (string split -m 1 = -- "$token")
                set prefix "$parts[1]="
                set token "$parts[2]"
            end
            # Fish's native path helper also interprets leading dashes as options.
            set -l search "$token"
            if string match -q -- '-*' "$token"
                set search "./$token"
            end
            set -l matches
            if string match -q ':dirs*' -- "$reply"
                set matches (__fish_complete_directories "$search")
            else
                set matches (__fish_complete_path "$search")
            end
            if test "$search" != "$token"
                set matches (string replace -r '^\./' '' -- $matches)
            end
            printf '%s\n' "$prefix"$matches
        case '*'
            printf '%s\n' $reply
    end
end

complete -c dots -f -a '(__dots_complete)'
