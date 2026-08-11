#!/usr/bin/env bash
# SPIKE(ADR-083) acceptance runs. Paired repeats, interleaved so any thermal /
# GPU-contention drift hits all arms equally rather than one whole arm.
set -u
SP=/private/tmp/claude-501/-Users-johan-Spelunk/7e57206e-8867-42ac-b4ff-d522d095dcd7/scratchpad
. "$SP/adr083/env.sh"
D="$SP/adr083"
mkdir -p "$D/runs"

for i in 1 2 3; do
  echo "########## REPEAT $i ##########"
  echo "--- only-code r$i ---"
  python3 "$D/eval083.py" --corpus-dir "$D" --arm only-code \
      --out "$D/runs/onlycode-r$i.json" 2>/dev/null
  echo "--- ungated default r$i ---"
  python3 "$D/eval083.py" --corpus-dir "$D" --arm default \
      --out "$D/runs/ungated-r$i.json" 2>/dev/null
  echo "--- gated T=1.2032 r$i ---"
  python3 "$D/eval083.py" --corpus-dir "$D" --arm default --gate 1.2032 \
      --out "$D/runs/gated1.2032-r$i.json" 2>/dev/null
done

echo "########## BAND EDGES (criterion A only) ##########"
for T in 1.1822 1.2337 1.2752; do
  echo "--- gated T=$T ---"
  python3 "$D/eval083.py" --corpus-dir "$D" --arm default --gate "$T" \
      --out "$D/runs/gated$T-r1.json" 2>/dev/null
done
echo "ALL DONE"
