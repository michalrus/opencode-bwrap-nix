#!/usr/bin/env bash
set -o errexit
set -o nounset
set -o pipefail

start=$(pwd -P)
if git_root=$(git -C "$start" rev-parse --show-toplevel 2>/dev/null); then
  boundary=$(cd "$git_root" && pwd -P)
else
  boundary=$start
fi

directories=()
directory=$start
while true; do
  directories+=("$directory")
  if [[ $directory == "$boundary" ]]; then
    break
  fi

  parent=${directory%/*}
  [[ -n $parent ]] || parent=/
  if [[ $parent == "$directory" ]]; then
    break
  fi
  directory=$parent
done

# Broader instructions come first so files nearer cwd can specialize them.
for ((i = ${#directories[@]} - 1; i >= 0; i--)); do
  directory=${directories[i]}
  for name in AGENTS.md CLAUDE.md CONTEXT.md; do
    candidate="${directory%/}/$name"
    if [[ -f $candidate ]]; then
      printf '\n%s\n\n' '----'
      printf "> Project instructions from: \`%s\`\n\n" "$candidate"
      cat -- "$candidate"
      break
    fi
  done
done

printf '\n'
