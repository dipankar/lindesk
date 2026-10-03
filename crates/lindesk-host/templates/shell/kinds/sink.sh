record() { # record <notification line>: sink.record has no reply
    if [[ $1 =~ \"kind\":\"level\"[^}]*\"to\":\"red\" ]]; then
        echo "{{name}}: the system is under pressure (level red)" >&2
        # notify-send "lindesk" "The system is under heavy load"
    fi
}
