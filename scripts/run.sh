#!/usr/bin/env bash
# Dev-loop process manager: pid files + logs under .run/, one component per
# name. Used by the lifecycle tasks in .taskfiles/lifecycle.yml.
#
#   scripts/run.sh start <name> <shell command>
#   scripts/run.sh stop <name> [name...]
#   scripts/run.sh stop-all
#   scripts/run.sh status
#   scripts/run.sh wait-for <seconds> <url>
set -euo pipefail

RUN_DIR="${RUN_DIR:-.run}"
LOG_DIR="$RUN_DIR/logs"
mkdir -p "$LOG_DIR"

pid_file() { printf '%s/%s.pid' "$RUN_DIR" "$1"; }

is_running() {
  local f p
  f="$(pid_file "$1")"
  [ -f "$f" ] || return 1
  p="$(cat "$f" 2>/dev/null || true)"
  [ -n "$p" ] && kill -0 "$p" 2>/dev/null
}

pid_of() { cat "$(pid_file "$1")" 2>/dev/null || true; }

start() {
  local name="$1" cmd="$2"
  if is_running "$name"; then
    echo "$name already running (pid $(pid_of "$name"))"
    return 0
  fi
  # setsid gives the command its own process group so `stop` can kill the
  # whole tree (gradle workers, vite children) with one signal.
  setsid bash -c "$cmd" >>"$LOG_DIR/$name.log" 2>&1 &
  echo $! >"$(pid_file "$name")"
  echo "started $name (pid $!) — log: $LOG_DIR/$name.log"
}

stop_one() {
  local name="$1" p f
  f="$(pid_file "$name")"
  if ! is_running "$name"; then
    rm -f "$f"
    return 0
  fi
  p="$(pid_of "$name")"
  kill -- -"$p" 2>/dev/null || kill "$p" 2>/dev/null || true
  for _ in $(seq 1 50); do
    kill -0 "$p" 2>/dev/null || break
    sleep 0.1
  done
  if kill -0 "$p" 2>/dev/null; then
    kill -9 -- -"$p" 2>/dev/null || kill -9 "$p" 2>/dev/null || true
  fi
  rm -f "$f"
  echo "stopped $name"
}

status() {
  local any=0 f name p
  for f in "$RUN_DIR"/*.pid; do
    [ -e "$f" ] || continue
    any=1
    name="$(basename "$f" .pid)"
    p="$(cat "$f")"
    if kill -0 "$p" 2>/dev/null; then
      echo "$name: running (pid $p)"
    else
      echo "$name: stale pid $p — run: task $name:stop"
    fi
  done
  [ "$any" = 1 ] || echo "nothing running"
}

wait_for() {
  local seconds="$1" url="$2" n=0
  while [ "$n" -lt "$seconds" ]; do
    curl -sf -m 2 "$url" >/dev/null 2>&1 && return 0
    n=$((n + 1))
    sleep 1
  done
  echo "timed out waiting for $url" >&2
  return 1
}

case "${1:-}" in
  start) start "$2" "$3" ;;
  stop) shift; for n in "$@"; do stop_one "$n"; done ;;
  stop-all) for f in "$RUN_DIR"/*.pid; do [ -e "$f" ] && stop_one "$(basename "$f" .pid)"; done ;;
  status) status ;;
  wait-for) wait_for "$2" "$3" ;;
  *) echo "usage: run.sh {start|stop|stop-all|status|wait-for}" >&2; exit 2 ;;
esac
