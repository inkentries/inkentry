use std::collections::HashMap;

/// Personalised PageRank over a bipartite entity↔chunk graph for LinearRAG Stage 2.
///
/// `edges` contains undirected pairs `(node_a, node_b)` (caller should add both
/// directions). `personalisation` maps entity names to activation scores — only
/// entity nodes need entries; chunk nodes start at zero personalisation weight.
/// Returns score per node name (both entity and chunk nodes).
pub fn compute_personalised_pagerank(
    edges: &[(String, String)],
    personalisation: &HashMap<String, f32>,
    iterations: usize,
    damping: f32,
) -> HashMap<String, f32> {
    if edges.is_empty() {
        return personalisation
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
    }

    // 1. Collect unique nodes; personalisation nodes must be present.
    //
    // Indices are assigned in sorted name order because index assignment fixes
    // the order the f32 sums in step 5 accumulate, and f32 addition is not
    // associative. Deriving them from `personalisation.keys()` and the caller's
    // edge order let both HashMap reseeding and edge emission order perturb the
    // scores themselves, not merely the order equal scores came back in.
    let mut names: Vec<&str> = personalisation.keys().map(String::as_str).collect();
    for (from, to) in edges {
        names.push(from.as_str());
        names.push(to.as_str());
    }
    names.sort_unstable();
    names.dedup();

    let mut nodes: Vec<String> = Vec::with_capacity(names.len());
    let mut node_index: HashMap<String, usize> = HashMap::with_capacity(names.len());
    for name in names {
        node_index.insert(name.to_string(), nodes.len());
        nodes.push(name.to_string());
    }

    let n = nodes.len();
    if n == 0 {
        return HashMap::new();
    }

    // 2. Adjacency lists.
    let mut out_edges: Vec<Vec<usize>> = vec![vec![]; n];
    let mut in_edges: Vec<Vec<usize>> = vec![vec![]; n];
    for (from, to) in edges {
        let fi = node_index[from];
        let ti = node_index[to];
        if fi != ti {
            out_edges[fi].push(ti);
            in_edges[ti].push(fi);
        }
    }

    // Pin the summation order of step 5 to node index rather than to the order
    // the caller happened to emit edges in, so a reordered edge list yields
    // bit-identical scores instead of merely similar ones.
    for ins in &mut in_edges {
        ins.sort_unstable();
    }

    // 3. Normalised personalisation vector. Summed over the sorted node order
    // for the same reason: `personalisation.values()` is HashMap-ordered, and a
    // divisor that shifts with it moves every score downstream.
    let total_p: f32 = nodes
        .iter()
        .filter_map(|name| personalisation.get(name))
        .copied()
        .sum::<f32>()
        .max(1e-9);
    let mut p_vec: Vec<f32> = vec![0.0; n];
    for (name, &score) in personalisation {
        if let Some(&idx) = node_index.get(name) {
            p_vec[idx] = score / total_p;
        }
    }

    // 4. Initialise scores.
    let init = 1.0_f32 / n as f32;
    let mut scores: Vec<f32> = vec![init; n];

    let dangling_indices: Vec<usize> = (0..n).filter(|&i| out_edges[i].is_empty()).collect();

    // 5. Power iterations with personalisation.
    for _ in 0..iterations {
        let dangling_sum: f32 = dangling_indices.iter().map(|&i| scores[i]).sum();

        // Teleport + dangling redistribution goes to personalisation nodes.
        let mut new_scores: Vec<f32> = vec![0.0; n];
        for i in 0..n {
            new_scores[i] += (1.0 - damping) * p_vec[i];
            new_scores[i] += damping * dangling_sum * p_vec[i];
        }

        for v in 0..n {
            for &u in &in_edges[v] {
                let out_deg = out_edges[u].len() as f32;
                new_scores[v] += damping * scores[u] / out_deg;
            }
        }

        scores = new_scores;
    }

    nodes
        .into_iter()
        .enumerate()
        .map(|(i, name)| (name, scores[i]))
        .collect()
}

/// Compute PageRank scores for a set of nodes given directed edges.
/// Returns a map from node name -> score (scores sum to ~1.0).
pub fn compute_pagerank(
    edges: &[(String, String)], // (from, to) symbol name pairs
    iterations: usize,          // typically 20
    damping: f32,               // typically 0.85
) -> HashMap<String, f32> {
    if edges.is_empty() {
        return HashMap::new();
    }

    // 1. Collect unique nodes
    let mut nodes: Vec<String> = Vec::new();
    let mut node_index: HashMap<String, usize> = HashMap::new();

    for (from, to) in edges {
        if !node_index.contains_key(from) {
            node_index.insert(from.clone(), nodes.len());
            nodes.push(from.clone());
        }
        if !node_index.contains_key(to) {
            node_index.insert(to.clone(), nodes.len());
            nodes.push(to.clone());
        }
    }

    let n = nodes.len();
    if n == 0 {
        return HashMap::new();
    }

    // 2. Build adjacency: out_edges[i] = list of target indices
    let mut out_edges: Vec<Vec<usize>> = vec![vec![]; n];
    // in_edges[i] = list of source indices
    let mut in_edges: Vec<Vec<usize>> = vec![vec![]; n];

    for (from, to) in edges {
        let fi = node_index[from];
        let ti = node_index[to];
        if fi != ti {
            out_edges[fi].push(ti);
            in_edges[ti].push(fi);
        }
    }

    // 3. Initialise scores: 1.0 / n for each node
    let init_score = 1.0_f32 / n as f32;
    let mut scores: Vec<f32> = vec![init_score; n];

    let base = (1.0 - damping) / n as f32;
    let dangling_indices: Vec<usize> = (0..n).filter(|&i| out_edges[i].is_empty()).collect();

    // 4. Iterate
    for _ in 0..iterations {
        let dangling_sum: f32 = dangling_indices.iter().map(|&i| scores[i]).sum();
        let dangling_contrib = damping * dangling_sum / n as f32;

        let mut new_scores: Vec<f32> = vec![base + dangling_contrib; n];

        for v in 0..n {
            for &u in &in_edges[v] {
                let out_deg = out_edges[u].len() as f32;
                new_scores[v] += damping * scores[u] / out_deg;
            }
        }

        scores = new_scores;
    }

    // 5. Return final scores
    nodes
        .into_iter()
        .enumerate()
        .map(|(i, name)| (name, scores[i]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Activations chosen so the f32 sums do not land on exact binary values —
    // reassociating them changes the low bits, which is the whole point.
    fn personalisation() -> HashMap<String, f32> {
        let mut p = HashMap::new();
        for (i, w) in [0.31_f32, 0.77, 0.13, 0.59, 0.41, 0.97]
            .into_iter()
            .enumerate()
        {
            p.insert(format!("sym_{i}"), w);
        }
        p
    }

    fn bipartite_edges() -> Vec<(String, String)> {
        let mut edges = Vec::new();
        for chunk in 0..40_i64 {
            for k in 0..3 {
                let sym = format!("sym_{}", (chunk as usize + k) % 6);
                let node = format!("c:{chunk}");
                edges.push((node.clone(), sym.clone()));
                edges.push((sym, node));
            }
        }
        edges
    }

    fn bits(scores: &HashMap<String, f32>) -> Vec<(String, u32)> {
        let mut out: Vec<(String, u32)> = scores
            .iter()
            .map(|(k, v)| (k.clone(), v.to_bits()))
            .collect();
        out.sort();
        out
    }

    // Issue #47: PPR scores must be a function of the graph, not of the order
    // the caller emitted edges in or the order a HashMap yielded its keys.
    // Equality is bitwise — "close enough" is what let a chunk's distance swing
    // ~2% between runs and reorder results downstream.
    #[test]
    fn personalised_pagerank_is_bitwise_stable_across_input_orderings() {
        let forward = bipartite_edges();
        let mut reversed = forward.clone();
        reversed.reverse();

        // A fresh HashMap each call: `RandomState` reseeds per instance, so
        // these iterate in different orders within this one process.
        let a = compute_personalised_pagerank(&forward, &personalisation(), 20, 0.85);
        let b = compute_personalised_pagerank(&reversed, &personalisation(), 20, 0.85);
        let c = compute_personalised_pagerank(&forward, &personalisation(), 20, 0.85);

        assert_eq!(
            bits(&a),
            bits(&b),
            "reordering the edge list must not move a single bit of any score"
        );
        assert_eq!(bits(&a), bits(&c), "repeated identical calls must agree");
    }
}
