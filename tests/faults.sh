#!/bin/sh
# Feed the published CLI format from one mirror in several fault states.
set -eu
binary=$1
fixture=$(mktemp -d)
trap 'rm -rf "$fixture"' EXIT
export fixture PATH="$fixture:$PATH"
cat > "$fixture/zpool" <<'MOCK'
#!/bin/sh
case $1 in
list) cat "$fixture/list" ;;
status)
    if [ "$scenario" = timeout ]; then echo "$$" > "$fixture/status.pid"; exec sleep 60; fi
    cat "$fixture/status"
    ;;
iostat)
    stamp=0
    while :; do
        echo "$stamp"
        cat "$fixture/block"
        stamp=$((stamp + 1))
        sleep 0.12
    done
    ;;
esac
MOCK
chmod +x "$fixture/zpool"
export scenario
for scenario in degraded replacing resilver finished invalid_size invalid_alloc timeout; do
    size=4000 alloc=1000 health=ONLINE state=FINISHED
    case $scenario in
        degraded|replacing) health=DEGRADED ;;
        resilver) state=SCANNING ;;
        invalid_size) size=bad ;;
        invalid_alloc) alloc=bad ;;
    esac
    printf 'tank\t%s\t%s\t3000\t0\t25\t%s\n' "$size" "$alloc" "$health" > "$fixture/list"
    old='"old":{"vdev_type":"disk","path":"/dev/old","state":"FAULTED","read_errors":9007199254740993,"checksum_errors":1}'
    new='"new":{"vdev_type":"disk","path":"/dev/new","state":"ONLINE","read_errors":0,"write_errors":0,"checksum_errors":0}'
    member=$new
    printf 'tank 32 96 1 2 3 4\n  mirror-0 32 96 1 2 3 4\n' > "$fixture/block"
    case $scenario in
        degraded) member=$old; echo '    /dev/old 0 0 1 2 3 4' >> "$fixture/block" ;;
        replacing)
            member='"replacing-0":{"vdev_type":"replacing","name":"replacing-0","state":"DEGRADED","vdevs":{'$old','$new'}}'
            printf '    replacing-0 0 0 1 2 3 4\n      /dev/old 0 0 1 2 3 4\n      /dev/new 0 0 1 2 3 4\n' >> "$fixture/block" ;;
        *) echo '    /dev/new 0 0 1 2 3 4' >> "$fixture/block" ;;
    esac
    echo '    /dev/peer 0 0 1 2 3 4' >> "$fixture/block"
    cat > "$fixture/status" <<EOF
{"pools":{"tank":{"scan_stats":{"function":"RESILVER","state":"$state","to_examine":4000,"issued":1500},
"vdevs":{"tank":{"vdev_type":"root","name":"tank","state":"$health","vdevs":{
"mirror-0":{"vdev_type":"mirror","name":"mirror-0","state":"$health","vdevs":{
"peer":{"vdev_type":"disk","path":"/dev/peer","state":"ONLINE"},$member}}}}}}}}
EOF
    if "$binary" -c /dev/null --once -i 250 >"$fixture/out" 2>"$fixture/err"; then
        grep -q "^# pool tank health=$health " "$fixture/out"
        grep -Eq '^ +peer +ONLINE .* -/-/-$' "$fixture/out"
        case $scenario in
            degraded|replacing) grep -Eq '^ +old +FAULTED .* 9007199254740993/-/1$' "$fixture/out" ;;
            resilver|finished) grep -Eq '^ +new +ONLINE .* 0/0/0$' "$fixture/out" ;;
            *) exit 1 ;;
        esac
        if [ "$scenario" = replacing ]; then
            grep -Eq '^ +replacing-0 +DEGRADED ' "$fixture/out"
            grep -Eq '^ +new +ONLINE ' "$fixture/out"
        fi
        if [ "$scenario" = resilver ]; then grep -q ' resilver=37.5%' "$fixture/out"
        elif grep -q 'resilver=' "$fixture/out"; then exit 1; fi
    else
        [ "$?" -eq 1 ] && [ ! -s "$fixture/out" ] || exit 1
        case $scenario in
            invalid_size) grep -q 'tank: invalid or missing size' "$fixture/err" ;;
            invalid_alloc) grep -q 'tank: invalid or missing alloc' "$fixture/err" ;;
            timeout)
                grep -q 'zpool status: timed out' "$fixture/err"
                if kill -0 "$(cat "$fixture/status.pid")" 2>/dev/null; then exit 1; fi ;;
            *) cat "$fixture/err" >&2; exit 1 ;;
        esac
    fi
done
