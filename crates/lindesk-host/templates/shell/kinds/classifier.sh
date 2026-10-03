# Commands classified as batch work (a bash regex).
BATCH_RE='^(rsync|tar|xz|zstd|gzip|borg|restic)$'

classify() { # classify <id> <request line>
    local comm
    comm=$(field comm "$2")
    if [[ $comm =~ $BATCH_RE ]]; then
        reply "$1" '{"class":"batch","confidence":0.9,"reason":"'"$comm"' is a bulk copy/compression tool"}'
    else
        reply "$1" null # no opinion: the next classifier decides
    fi
}
