#!/bin/sh
# Used through a temporary PATH entry by the local-review signal regression.
phase=metadata
for argument in "$@"; do
    case "$argument" in
        --name-only) phase=names ;;
        cat-file) phase=blob ;;
    esac
done
if [ "$(cat guidance-phase)" = "$phase" ]; then
    echo $$ > guidance.pid
    exec sleep 30
fi
case "$phase" in
    names) printf 'AGENTS.md\000' ;;
    metadata) printf '100644 blob aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\tAGENTS.md\000' ;;
    *) exit 17 ;;
esac
