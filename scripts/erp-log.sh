#!/usr/bin/env bash
# Find the server log lines of one request (SPEC-ERRTRACE, runbook).
#
# <request-id> is the UUID the error dialog shows (the X-Request-Id of the response). Prints every
# line that carries it, in the order written: the call's own failure line, a WebAssembly addon's
# line, the browser's report of it (reported_request_id=...). Rotated files next to the log
# (server.stderr.log.1, ...) are searched too. Exit status: 0 found, 1 no line (the id is
# valid but nothing was logged: a success, an old rotation, another host, or a log level below
# the line), 2 usage or unreadable log.
set -euo pipefail

usage() {
  echo "usage: ${0##*/} <request-id>   (ERP_LOG overrides the log path)" >&2
  exit 2
}

[ $# -eq 1 ] || usage
id=$(printf '%s' "$1" | tr 'A-F' 'a-f')
# strict: a UUID in text form, so the id is safe to pass to grep and nothing else is searched for
if ! printf '%s' "$id" | grep -Eq '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'; then
  echo "${0##*/}: '$1' is not a request id (a UUID like 0194f5a0-7b1c-7d2e-8f3a-123456789abc)" >&2
  exit 2
fi

log="${ERP_LOG:-$HOME/.local/state/erpforall-human/server.stderr.log}"
if [ ! -r "$log" ]; then
  echo "${0##*/}: cannot read $log (set ERP_LOG to the server's stderr log on this host)" >&2
  exit 2
fi

found=1
# oldest rotation first, the live file last, so the output is in the order written
files=()
for rotated in $(ls -1 "$log".* 2>/dev/null | sort -r); do
  [ -r "$rotated" ] && files+=("$rotated")
done
files+=("$log")
for file in "${files[@]}"; do
  case "$file" in
    *.gz) matches=$(gzip -dc -- "$file" | grep -F -- "$id" || true) ;;
    *) matches=$(grep -F -- "$id" "$file" || true) ;;
  esac
  if [ -n "$matches" ]; then
    printf '%s\n' "$matches"
    found=0
  fi
done
if [ "$found" -ne 0 ]; then
  echo "${0##*/}: no line for $id in ${files[*]}" >&2
fi
exit "$found"
