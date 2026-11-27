#!/bin/sh
# Exercise the real CLI with a deterministic zpool stream.
set -eu
binary=$1
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export PATH="$work:$PATH"
cat > "$work/zpool" <<'MOCK'
#!/bin/sh
case $1 in
list)
    if [ "$scenario" = list_error ]; then echo 'cannot read pool' >&2; exit 1; fi
    printf 'tank\t128\t32\t96\t0\t25\tONLINE\n'
    ;;
status)
    case $scenario in
        unsupported) echo "invalid option 'j'" >&2; exit 2 ;;
        bad_json) echo 'unreadable'; exit 0 ;;
    esac
    echo '{"pools":{"tank":{}}}'
    ;;
iostat)
    for stamp in 1 2 3; do
        echo "$stamp"
        echo 'tank 32 96 1 2 3 4'
        echo '  mirror-0 32 96 1 2 3 4'
        echo '    /dev/member 0 0 1 2 3 4'
        echo '  /dev/top 0 0 1 2 3 4'
        if [ "$scenario" = stalled ]; then exec sleep 20; fi
        if [ "$scenario" = stopped ]; then exit 0; fi
        sleep 0.12
    done
    exec sleep 2
    ;;
esac
MOCK
chmod +x "$work/zpool"
export scenario
for scenario in healthy unsupported stopped stalled list_error bad_json; do
    if "$binary" -c /dev/null --once --vdevs-only -i 250 >"$work/out" 2>"$work/err"; then
        case $scenario in healthy|unsupported) ;; *) exit 1 ;; esac
        grep -q '^# pool tank health=ONLINE' "$work/out"
        grep -q 'mirror-0' "$work/out"
        grep -q 'top' "$work/out"
        if grep -q 'member' "$work/out"; then exit 1; fi
        if [ "$scenario" = unsupported ]; then grep -q 'zpool status: invalid option' "$work/err"; fi
    else
        [ "$?" -eq 1 ]
        [ ! -s "$work/out" ]
        case $scenario in
            stopped) grep -q 'zpool iostat ended' "$work/err" ;;
            stalled) grep -q 'incomplete sample' "$work/err" ;;
            list_error) grep -q 'zpool list: cannot read pool' "$work/err" ;;
            bad_json) grep -q 'zpool status: invalid JSON' "$work/err" ;;
            *) exit 1 ;;
        esac
    fi
done
