#!/bin/sh
printf '%s\n' "$*" >> "$MOCK_DIR/calls"
case "$1" in
 ps) cat "$MOCK_DIR/containers.json" ;;
 images) cat "$MOCK_DIR/images.json" ;;
 rm) test ! -e "$MOCK_DIR/fail-remove" ;;
 image) test ! -e "$MOCK_DIR/fail-remove" ;;
 *) exit 7 ;;
esac
