#!/usr/bin/env bash
# Tabulate every job's results.tsv under $1 per variant: runs, failed runs,
# failures by symptom and which runs failed. Writes the table to the job
# summary and the log, and one notice per variant (shown by `gh run view`).
set -u
dir=${1:?results directory}

mapfile -t files < <(find "$dir" -name results.tsv | sort)
if [ "${#files[@]}" = 0 ]; then
  echo "::error::no results.tsv found under $dir"
  exit 1
fi

table=$(awk -F'\t' '
  FNR == 1 { next }
  {
    v = $1
    if (!(v in runs)) order[++n] = v
    runs[v]++
    if ($4 != "pass") { failed[v]++; which[v] = which[v] " s" $2 ":" $3 }
    if ($4 ~ /hello-eio/) eio[v]++
    if ($4 ~ /xattr-eperm/) eperm[v]++
    if ($4 ~ /timeout/) tmo[v]++
    if ($4 == "other") other[v]++
    if ($6 > 0) linger[v]++
  }
  END {
    print "| variant | runs | failed | hello EIO | xattr EPERM | timeout | other | helper alive at exit | failed runs (shard:run) |"
    print "| --- | --- | --- | --- | --- | --- | --- | --- | --- |"
    for (i = 1; i <= n; i++) {
      v = order[i]
      printf "| %s | %d | %d | %d | %d | %d | %d | %d | %s |\n", v, runs[v], failed[v], eio[v], eperm[v], tmo[v], other[v], linger[v], which[v]
    }
  }' "${files[@]}")

echo "$table"
{
  echo "## macfuse-exp results"
  echo
  echo "$table"
} >>"${GITHUB_STEP_SUMMARY:-/dev/null}"

echo "$table" | awk -F' *\\| *' 'NR > 2 {
  printf "::notice title=%s::%s runs, %s failed (hello EIO %s, xattr EPERM %s, timeout %s, other %s)%s\n", $2, $3, $4, $5, $6, $7, $8, ($10 == "" ? "" : "; failed runs:" $10)
}'
