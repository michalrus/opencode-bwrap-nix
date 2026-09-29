# Runs one Playwright MCP server on a private headless Chromium whose profile
# lives in a temporary directory that is removed when the server exits.
#
# Inputs (set by the Nix wrapper):
#   PLAYWRIGHT_MCP            path to the playwright-mcp executable
#   PLAYWRIGHT_MCP_CONFIG     JSON config template with `@PLAYWRIGHT_…@` tokens
#   PLAYWRIGHT_EXTENSIONS     newline-separated unpacked extension directories
#   PLAYWRIGHT_PLACEHOLDERS   newline-separated env var names; every `@NAME@`
#                             inside the staged extensions becomes `$NAME`

state_dir=$(mktemp -d -t playwright-mcp.XXXXXXXXXX)
mcp_pid=

# shellcheck disable=SC2329 # invoked by the traps below
cleanup() {
  rm -rf -- "$state_dir"
}

# Forward termination to the MCP child so its Chromium exits first and the
# EXIT trap can then remove a directory nothing holds open anymore.
# shellcheck disable=SC2329 # invoked by the traps below
forward() {
  if [ -n "$mcp_pid" ]; then
    kill -TERM "$mcp_pid" 2>/dev/null || true
  fi
}

trap cleanup EXIT
trap forward TERM INT HUP

profile_dir="$state_dir/profile"
mkdir -m 700 "$profile_dir"

# Extensions come from the read-only Nix store, but Chromium wants to write
# into unpacked extension directories, and some carry secrets that must never
# enter the store: stage a private copy and fill in the placeholders at
# startup.
load_extension=
index=0
while IFS= read -r ext; do
  [ -n "$ext" ] || continue
  index=$((index + 1))
  ext_dir="$state_dir/ext-$index"
  cp -r --no-preserve=mode,ownership -- "$ext" "$ext_dir"
  chmod -R u+w "$ext_dir"

  while IFS= read -r var; do
    [ -n "$var" ] || continue
    if [ -z "${!var-}" ]; then
      printf >&2 '%s: %s is unset or empty; leaving @%s@ in %s as is\n' "${0##*/}" "$var" "$var" "$ext"
      continue
    fi
    replacement=$(printf '%s' "${!var}" | sed -e 's/[&|\\]/\\&/g')
    # `grep -l` exits non-zero when the placeholder is absent, which is fine.
    { find "$ext_dir" -type f -exec grep -lZ -F -- "@$var@" {} + 2>/dev/null || true; } |
      xargs -0 -r sed -i "s|@$var@|$replacement|g"
  done <<<"${PLAYWRIGHT_PLACEHOLDERS-}"

  load_extension="${load_extension:+$load_extension,}$ext_dir"
done <<<"${PLAYWRIGHT_EXTENSIONS-}"

config_file="$state_dir/config.json"
jq \
  --arg profile "$profile_dir" \
  --arg extensions "$load_extension" \
  '
    .browser.userDataDir = $profile
    | .browser.launchOptions.args |= map(
        if . == "--load-extension=@PLAYWRIGHT_EXTENSIONS@"
        then "--load-extension=" + $extensions
        else . end
      )
  ' "$PLAYWRIGHT_MCP_CONFIG" >"$config_file"

# Background jobs get `/dev/null` as stdin; the MCP speaks JSON-RPC over
# ours, so pass it on explicitly.
"$PLAYWRIGHT_MCP" --config "$config_file" "$@" <&0 &
mcp_pid=$!

# `wait` returns as soon as a trapped signal arrives, before the child has
# exited; keep waiting until it is really gone so the EXIT trap runs last.
status=0
while :; do
  if wait "$mcp_pid"; then
    status=0
  else
    status=$?
  fi
  kill -0 "$mcp_pid" 2>/dev/null || break
done
exit "$status"
