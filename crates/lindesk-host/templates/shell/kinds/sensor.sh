poll() { # poll <id> <request line>: report system CPU pressure from /proc/pressure/cpu
    local avg10="" now=0 psi=""
    if [[ $2 =~ \"now\":([0-9]+) ]]; then now=${BASH_REMATCH[1]}; fi
    if [ -r /proc/pressure/cpu ]; then psi=$(head -n1 /proc/pressure/cpu); fi
    if [[ $psi =~ avg10=([0-9.]+) ]]; then avg10=${BASH_REMATCH[1]}; fi
    if [ -n "$avg10" ]; then
        reply "$1" '{"samples":[{"ts":'"$now"',"scope":{"type":"system"},"resource":"cpu","some_avg10":'"$avg10"',"some_delta_us":0,"interval_ms":0}]}'
    else
        reply "$1" '{"samples":[]}'
    fi
}
