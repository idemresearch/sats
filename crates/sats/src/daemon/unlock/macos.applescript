on run argv
    try
        set answer to display dialog (item 1 of argv) with title "sats — Unlock wallet" default answer "" with hidden answer buttons {"Cancel", "Unlock"} default button "Unlock" cancel button "Cancel" giving up after 120
        if gave up of answer then return "timed_out"
        set passwordText to text returned of answer
        if (count characters of passwordText) > 1024 then return "failed"
        return "password:" & passwordText
    on error number errorNumber
        if errorNumber is -128 then return "cancelled"
        return "failed"
    end try
end run
