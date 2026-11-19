#!/usr/bin/env bash
# Run the macfuse_smoke test binary $REPEATS times on this runner and record
# each run's outcome in $OUT/results.tsv. Failing runs keep go-nfsv4's debug
# log, the unified log around the failure and the helper/mount state next to
# the test output in $OUT.
#
# Env: VARIANT, SHARD, REPEATS, OUT (set by the workflow).
set -u
: "${VARIANT:?}" "${SHARD:?}" "${REPEATS:?}" "${OUT:?}"

# go-nfsv4's -d debug log hides some failures, so the PR check runs without
# it, against baseline-nolog: master's code, also without it.
case $VARIANT in
  baseline-nolog | pr-fix) ;;
  *) export FBR_FUSE_T_DEBUG=1 ;;
esac

FUSE_T_LOGS=$HOME/Library/Logs/fuse-t
RESULTS=$OUT/results.tsv
mkdir -p "$OUT"
printf 'variant\tshard\trep\tresult\tsecs\thelpers_at_exit\thelpers_after_5s\tnfs_mounts_after\n' >"$RESULTS"

bin=$(cargo test --features=fusedev,fuse-t --test macfuse_smoke --no-run --message-format=json |
  sed -n 's/.*"executable":"\([^"]*\/macfuse_smoke-[^"]*\)".*/\1/p' | head -1)
if [ ! -x "$bin" ]; then
  echo "::error::macfuse_smoke test binary not found"
  exit 1
fi

helpers() { pgrep -f go-nfsv4 | wc -l | tr -d ' '; }
nfs_mounts() { mount | grep -c '(nfs' || true; }

saved_pass_log=no
for rep in $(seq 1 "$REPEATS"); do
  base=$OUT/rep-$rep
  rm -rf "$FUSE_T_LOGS"
  mkdir -p "$FUSE_T_LOGS"

  start=$(date +%s)
  "$bin" --nocapture --test-threads=1 >"$base.txt" 2>&1 &
  pid=$!
  # A hung request on fuse-t's hard NFS mount never returns; give up on the
  # run after 150 s instead of eating the job's timeout.
  (sleep 150 && kill -KILL "$pid" 2>/dev/null && echo "e2e harness: killed after 150 s" >>"$base.txt") \
    </dev/null >/dev/null 2>&1 &
  watchdog=$!
  wait "$pid"
  rc=$?
  # Killing the watchdog's sleep ends the watchdog without it firing.
  pkill -P "$watchdog" 2>/dev/null
  wait "$watchdog" 2>/dev/null
  secs=$(($(date +%s) - start))

  symptoms=""
  grep -q 'exec failed: cat .*Input/output error' "$base.txt" && symptoms+="hello-eio,"
  grep -q 'exec failed: xattr -w .*Operation not permitted' "$base.txt" && symptoms+="xattr-eperm,"
  grep -q 'e2e harness: killed after' "$base.txt" && symptoms+="timeout,"
  if [ "$rc" = 0 ]; then
    result=pass
  elif [ -n "$symptoms" ]; then
    result=${symptoms%,}
  else
    result=other
  fi

  at_exit=$(helpers)
  # Let this run's go-nfsv4 helpers exit before the next run starts.
  for _ in $(seq 50); do
    [ "$(helpers)" = 0 ] && break
    sleep 0.1
  done
  after_5s=$(helpers)

  if [ "$result" != pass ] || [ "$saved_pass_log" = no ]; then
    cp -R "$FUSE_T_LOGS" "$base-fuse-t" 2>/dev/null
    {
      echo "rc=$rc result=$result secs=$secs helpers_at_exit=$at_exit helpers_after_5s=$after_5s"
      # shellcheck disable=SC2009 # ps for the state and elapsed-time columns
      ps -axo pid,ppid,stat,etime,command | grep -i '[g]o-nfsv4'
      mount | grep '(nfs'
    } >"$base-state.txt" 2>&1
    [ "$result" = pass ] && saved_pass_log=yes
  fi
  if [ "$result" != pass ]; then
    # shellcheck disable=SC2024 # the file is ours; only log show needs root
    sudo log show --last 2m --style compact --info --debug --predicate \
      '(process == "kernel" AND (eventMessage CONTAINS[c] "nfs" OR eventMessage CONTAINS[c] "xattr" OR eventMessage CONTAINS[c] "sandbox")) OR process == "doubleagentd" OR process CONTAINS[c] "go-nfsv4" OR process == "mount_nfs" OR process == "tccd"' \
      >"$base-unified.log" 2>&1
  fi

  # Clean up whatever is left so runs stay independent: stuck helpers and
  # their NFS mounts.
  if [ "$after_5s" != 0 ]; then
    pkill -9 -f go-nfsv4
  fi
  mount | sed -n 's/^.* on \(\/private\/var\/folders\/.*\) (nfs.*$/\1/p' | while read -r mp; do
    sudo umount -f "$mp" 2>/dev/null || sudo diskutil unmount force "$mp" >/dev/null 2>&1
  done
  mounts_after=$(nfs_mounts)

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$VARIANT" "$SHARD" "$rep" "$result" "$secs" \
    "$at_exit" "$after_5s" "$mounts_after" >>"$RESULTS"
  echo "run $rep: $result (${secs}s, helpers at exit $at_exit, after 5s $after_5s)"
done

runs=$(awk -F'\t' 'NR > 1' "$RESULTS" | wc -l | tr -d ' ')
failed=$(awk -F'\t' 'NR > 1 && $4 != "pass"' "$RESULTS" | wc -l | tr -d ' ')
reps=$(awk -F'\t' 'NR > 1 && $4 != "pass" { printf "%s%s:%s", sep, $3, $4; sep = " " }' "$RESULTS")
echo "::notice title=$VARIANT s$SHARD::$failed of $runs runs failed${reps:+: $reps}"
{
  echo "### $VARIANT, shard $SHARD: $failed of $runs runs failed"
  echo
  echo '| run | result | secs | helpers at exit | after 5 s | nfs mounts after |'
  echo '| --- | --- | --- | --- | --- | --- |'
  awk -F'\t' 'NR > 1 { printf "| %s | %s | %s | %s | %s | %s |\n", $3, $4, $5, $6, $7, $8 }' "$RESULTS"
} >>"${GITHUB_STEP_SUMMARY:-/dev/null}"
exit 0
