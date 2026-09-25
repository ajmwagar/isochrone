tell application "iTerm"
    activate
    tell current window
        create tab with default profile
        tell current session to write text "/tmp/run-isochrone-sender.command"
    end tell
end tell
