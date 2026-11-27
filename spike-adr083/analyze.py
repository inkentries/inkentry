#!/usr/bin/env python3
import json, math, statistics as st

M = json.load(open('/private/tmp/claude-501/-Users-johan-Spelunk/7e57206e-8867-42ac-b4ff-d522d095dcd7/scratchpad/matrix.json'))
neg = M['neg_matrix']          # 500 x 20
pos = M['pos_matrix']          # 20 x 20
pos_ids = M['pos_note_ids']
note_ids = M['note_ids']
idx = {n: i for i, n in enumerate(note_ids)}

flat_neg = sorted(d for row in neg for d in row)
paired_pos = sorted(pos[i][idx[pos_ids[i]]] for i in range(len(pos_ids)))
per_query_min = sorted(min(row) for row in neg)

def pct(sorted_vals, p):
    if not sorted_vals: return float('nan')
    k = (len(sorted_vals) - 1) * p / 100.0
    lo, hi = math.floor(k), math.ceil(k)
    if lo == hi: return sorted_vals[int(k)]
    return sorted_vals[lo] * (hi - k) + sorted_vals[hi] * (k - lo)

def cos(d): return 1 - d*d/2

print("=" * 78)
print("NEGATIVES: 500 CSN queries x 20 memory notes = %d pairs" % len(flat_neg))
print("=" * 78)
for p in [0, 0.01, 0.05, 0.1, 0.25, 0.5, 1, 2, 5, 10, 25, 50, 75, 95, 100]:
    v = pct(flat_neg, p)
    print(f"  p{p:<6} L2={v:.4f}  cos={cos(v):+.4f}   (pairs at or below: {int(round(p/100*len(flat_neg)))})")
print(f"  mean={st.mean(flat_neg):.4f} sd={st.pstdev(flat_neg):.4f}")

print()
print("=" * 78)
print("POSITIVES: %d paired (question, note) distances" % len(paired_pos))
print("=" * 78)
for p in [0, 5, 10, 25, 50, 75, 90, 95, 100]:
    v = pct(paired_pos, p)
    print(f"  p{p:<6} L2={v:.4f}  cos={cos(v):+.4f}")
print(f"  mean={st.mean(paired_pos):.4f} sd={st.pstdev(paired_pos):.4f}")
print("  all:", " ".join(f"{d:.3f}" for d in paired_pos))

print()
print("=" * 78)
print("OVERLAP")
print("=" * 78)
pos_max, pos_min = max(paired_pos), min(paired_pos)
neg_min, neg_max = flat_neg[0], flat_neg[-1]
print(f"  positive range   [{pos_min:.4f}, {pos_max:.4f}]")
print(f"  negative range   [{neg_min:.4f}, {neg_max:.4f}]")
gap = neg_min - pos_max
print(f"  clean separation? {'YES' if gap > 0 else 'NO'}   (neg_min - pos_max = {gap:+.4f})")
# how many negatives fall below the positive median / max
for label, thr in [("pos p50", pct(paired_pos,50)), ("pos p90", pct(paired_pos,90)), ("pos max", pos_max)]:
    below = sum(1 for d in flat_neg if d <= thr)
    print(f"  negatives at or below {label} ({thr:.4f}): {below} / {len(flat_neg)} = {below/len(flat_neg)*100:.3f}%")
# overlapping region mass
print(f"  positives above neg p0.1 ({pct(flat_neg,0.1):.4f}): "
      f"{sum(1 for d in paired_pos if d > pct(flat_neg,0.1))} / {len(paired_pos)}")

print()
print("=" * 78)
print("BAND SWEEP  (threshold T = admit if L2 <= T)")
print("=" * 78)
print(f"{'source':<16}{'T':>8}{'cos':>9}{'pos_rec':>9}{'pos_top1':>9}"
      f"{'adm_pairs':>11}{'mean/query':>12}{'q_with_any':>11}")
rows = []
for p in [0.01, 0.02, 0.05, 0.1, 0.2, 0.25, 0.5, 1.0, 2.0, 5.0]:
    rows.append((f"neg p{p}", pct(flat_neg, p)))
for T in [0.90, 0.95, 1.00, 1.05, 1.10, 1.15, 1.20]:
    rows.append(("fixed", T))
rows.sort(key=lambda r: r[1])
for src, T in rows:
    # positive recall: paired note admitted
    rec = sum(1 for i in range(len(pos_ids)) if pos[i][idx[pos_ids[i]]] <= T)
    # positive top-1: paired note admitted AND nearest among survivors
    top1 = 0
    for i in range(len(pos_ids)):
        surv = [(pos[i][j], note_ids[j]) for j in range(len(note_ids)) if pos[i][j] <= T]
        if surv and min(surv)[1] == pos_ids[i]:
            top1 += 1
    admitted = sum(1 for row in neg for d in row if d <= T)
    qany = sum(1 for row in neg if min(row) <= T)
    print(f"{src:<16}{T:>8.4f}{cos(T):>9.4f}{rec:>6}/{len(pos_ids):<3}"
          f"{top1:>6}/{len(pos_ids):<3}{admitted:>11}{admitted/len(neg):>12.4f}{qany:>11}")

print()
print("=" * 78)
print("PER-QUERY MINIMUM negative distance (the statistic criterion A rides on)")
print("=" * 78)
for p in [0, 0.2, 1, 2, 5, 10, 25, 50, 100]:
    print(f"  p{p:<5} {pct(per_query_min, p):.4f}")

print()
print("=" * 78)
print("CRITERION A ARITHMETIC:  mean memory-in-top-10 = admitted_pairs / 500")
print("  (upper bound; a pair can only occupy a slot if admitted)")
print("=" * 78)
target = 0.05 * len(neg)   # 25 pairs
print(f"  <= 0.05 mean requires <= {target:.0f} admitted pairs out of {len(flat_neg)}")
print(f"  = negative percentile {target/len(flat_neg)*100:.3f}%  ->  T = {pct(flat_neg, target/len(flat_neg)*100):.4f}")
print(f"  ADR's proposed starting point (neg p1) admits {sum(1 for d in flat_neg if d <= pct(flat_neg,1))} pairs"
      f"  -> mean {sum(1 for d in flat_neg if d <= pct(flat_neg,1))/len(neg):.3f}")

print()
print("=" * 78)
print("PER-POSITIVE DETAIL (paired distance, rank among 20 notes, nearest rival)")
print("=" * 78)
for i, nid in enumerate(pos_ids):
    row = pos[i]
    order = sorted(range(len(row)), key=lambda j: row[j])
    rank = order.index(idx[nid]) + 1
    d = row[idx[nid]]
    rival_j = order[0] if order[0] != idx[nid] else order[1]
    print(f"  note {nid:>2}  d={d:.4f} (cos {cos(d):+.3f})  rank={rank:>2}/20   "
          f"nearest_rival=note {note_ids[rival_j]} d={row[rival_j]:.4f}")
