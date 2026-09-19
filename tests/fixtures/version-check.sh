#!/bin/sh
# Infrastructure fixture selected by its per-test symlink name.
case "${0##*/}" in
 success) printf 'crow 1.2.3\n' ;;
 failure) printf 'crow 1.2.3\n'; exit 1 ;;
 oversized) head -c 5000 /dev/zero ;;
 *) exit 2 ;;
esac
