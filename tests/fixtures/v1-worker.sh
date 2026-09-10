#!/bin/sh
# Fake agent CLI for v1 engine tests. Reads its prompt on stdin and obeys markers
# embedded in the task brief, so the dispatcher can be exercised without model credits.
#   CREATE:<name>  write that file in the current worktree
#   DUMP:<name>    write the whole prompt to that file, so tests can assert on what an
#                  agent was actually told rather than on what we think it was told
#   FAILNOW        exit non-zero without writing anything
#   (neither)      do nothing at all, and exit successfully
prompt=$(cat)

case "$prompt" in
  *FAILNOW*)
    echo "simulated agent failure" >&2
    exit 1
    ;;
esac

# Acting as the observer for a task that asks for a follow-up: propose one.
case "$prompt" in
  *"You are the observer"*propose-followup*)
    echo '{"entries":[],"proposals":[{"op":"add","task":{"id":"followup","title":"Follow up","brief":"CREATE:followup.txt","verify":["sh","-c","test -f followup.txt"],"depends_on":[]}}]}'
    exit 0 ;;
esac

# NOTE:<text> leaves a note for the team in the path the prompt names.
note=$(printf '%s\n' "$prompt" | sed -n 's/.*NOTE:\([^ ]*\).*/\1/p' | head -1)
if [ -n "$note" ]; then
  notes=$(printf '%s\n' "$prompt" | sed -n 's#.*append it to \(/[^,]*\.jsonl\),.*#\1#p' | head -1)
  [ -n "$notes" ] && printf '{"kind":"approach","title":"%s","body":"worth reusing"}\n' "$note" >> "$notes"
fi

dump=$(printf '%s\n' "$prompt" | sed -n 's/.*DUMP:\([A-Za-z0-9._-]\{1,\}\).*/\1/p' | head -1)
if [ -n "$dump" ]; then
  printf '%s\n' "$prompt" > "$dump"
  echo "dumped prompt to $dump"
fi

target=$(printf '%s\n' "$prompt" | sed -n 's/.*CREATE:\([A-Za-z0-9._-]\{1,\}\).*/\1/p' | head -1)
if [ -n "$target" ]; then
  echo "written by the fake agent" > "$target"
  echo "wrote $target"
else
  echo "nothing to do"
fi
exit 0
