#!/bin/sh
if [ "$1" = "--version" ]; then
    echo 'ssh-clipboard 0.2.12'
    exit 0
fi
echo "$$ $DISPLAY" > "$SSH_CLIPBOARD_STATE_DIR/test-child"
exec sleep 600
