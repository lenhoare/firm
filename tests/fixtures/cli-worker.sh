#!/bin/sh
# Local protocol fixture only: no network, models, or workspace writes.
prompt_file=''
while [ "$#" -gt 0 ]; do
    printf 'ARG: %s\n' "$1"
    if [ "$1" = '--prompt-file' ]; then
        shift
        prompt_file="$1"
        printf 'PROMPT_FILE: %s\n' "$prompt_file"
    fi
    shift
done
if [ -n "$prompt_file" ]; then
    cat "$prompt_file"
else
    cat
fi
