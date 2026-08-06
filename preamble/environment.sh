#!/usr/bin/env bash
set -o errexit
set -o nounset
set -o pipefail

echo
echo '## Environment'
echo
echo "System: $(uname -a)"
echo "Date: $(date --utc --rfc-email)"
echo "User: $(id)"
echo "Home directory: $HOME"
echo "Current working directory: $(pwd)"
echo "Git root: $(git rev-parse --show-toplevel 2>/dev/null || echo 'not in a Git repository')"
echo "Nix shell: $(if [ -n "${IN_NIX_SHELL:-}" ]; then echo yes; else echo no; fi)"
echo "Direnv: $(if [ -n "${DIRENV_FILE:-}" ]; then echo yes; else echo no; fi)"

echo
echo '## Repository'
echo
if git rev-parse --show-toplevel >/dev/null 2>&1; then
  onefetch --no-art --no-color-palette 2>&1 | ansifilter | sed -r '/^------+$/ d'
fi

echo '### Directory structure'
echo
echo '```'
git-tree-digest 150 3 | ansifilter
echo '```'

if git rev-parse --show-toplevel >/dev/null 2>&1; then
  echo
  echo '### Recent commits'
  echo
  git log --oneline -10 | ansifilter

  changes=$(git status --short)
  if [ -n "$changes" ]; then
    echo
    echo '### Uncommitted changes'
    echo
    git status --short | ansifilter
  fi
fi

echo
