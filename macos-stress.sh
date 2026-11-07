#!/bin/bash
# Runs the hand-off multipart test and the two OCI re-check deadline tests many times at once on
# this runner, and fails when any run failed. Not for main: it is the body of a by-hand stress run.
set -u
cargo test --locked -p tierstore --features tierstore/s3 --lib --no-run
bin=$(cargo test --locked -p tierstore --features tierstore/s3 --lib --no-run --message-format=json 2>/dev/null \
  | jq -r 'select(.reason == "compiler-artifact" and .profile.test == true and .target.name == "tierstore" and (.target.kind | index("lib"))) | .executable' | head -1)
echo "binary: $bin"
H=server::cache_handoff_traffic_tests::a_multipart_upload_goes_on_across_the_take_and_an_abort_removes_it
G=server::oci::pull::tests::a_get_of_a_moved_tag_that_is_never_answered_is_cut_at_the_recheck_deadline
D=server::oci::pull::tests::a_head_that_is_never_answered_serves_the_stored_tag_at_the_recheck_deadline
out="$RUNNER_TEMP/stress"
mkdir -p "$out"
for round in $(seq 1 "$ROUNDS"); do
  handoff=()
  for i in $(seq 1 "$HANDOFF_PAR"); do
    "$bin" --exact "$H" > "$out/handoff-$round-$i.log" 2>&1 & handoff+=($!)
  done
  batch=0
  while kill -0 "${handoff[@]}" 2>/dev/null; do
    batch=$((batch + 1))
    oci=()
    for i in $(seq 1 "$OCI_PAR"); do
      "$bin" --exact "$G" > "$out/get-$round-$batch-$i.log" 2>&1 & oci+=($!)
      "$bin" --exact "$D" > "$out/head-$round-$batch-$i.log" 2>&1 & oci+=($!)
    done
    wait "${oci[@]}"
  done
  wait "${handoff[@]}"
  echo "round $round done"
done
summary() {
  local prefix=$1
  local ok fail
  ok=$(grep -l 'test result: ok. 1 passed' "$out"/"$prefix"-*.log | wc -l | tr -d ' ')
  fail=$(grep -L 'test result: ok. 1 passed' "$out"/"$prefix"-*.log | wc -l | tr -d ' ')
  echo "$prefix: $ok passed, $fail failed"
  [ "$fail" = 0 ]
}
status=0
summary handoff || status=1
summary get || status=1
summary head || status=1
for f in $(grep -L 'test result: ok. 1 passed' "$out"/*.log); do
  echo "== $f"; grep -A4 panicked "$f"
done
exit $status
