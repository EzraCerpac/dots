# Load with: dots completions fish | source
function __dots_complete
    set -l prior (commandline -opc)
    set -l token (commandline -ct)
    set -l current (string unescape -- "$token")

    set -l reply (command dots internal shell-complete $prior[2..-1] "$current")
    set -q reply[1]; or return
    switch "$reply"
        case :files
            __fish_complete_path "$token"
        case :dirs
            __fish_complete_directories "$token"
        case '*'
            printf '%s\n' $reply
    end
end

complete -c dots -f -a '(__dots_complete)'
