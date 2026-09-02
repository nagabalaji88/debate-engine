//! Shared "cheap similarity" infrastructure — INTERFACES §3's own framing:
//! `SimilarityStack` is described as reusable "internal plane" machinery, and
//! two stages consume it: `claims.normalize`'s T1 (candidate generation for
//! clustering) and `relations.analyze`'s T1 (candidate generation for
//! relationship classification, plus its own T2 polarity sweep). Union-find,
//! the K-scaling top-K formula, and trigram-IDF cosine live here once rather
//! than twice (PLAN_DEVIATIONS.md D33, D35).

use arbiter_core::CanonicalClaim;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
pub(crate) struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    pub(crate) fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    pub(crate) fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]);
        }
        self.parent[x]
    }

    pub(crate) fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[ra] = rb;
        }
    }
}

/// INTERFACES §3's K-scaling formula, transcribed verbatim: `clamp(ceil(3.0 *
/// log2(n + 1)), 8, 24)`. The worked example table in that section (`n=12 ->
/// 11`, `n=32 -> 16`, ...) does not reproduce exactly under this literal
/// reading with straightforward rounding — plausibly a documentation
/// rounding inconsistency in the worked examples rather than in the formula
/// itself, which is given as an actual expression, not just examples. The
/// formula is transcribed as written rather than reverse-engineered from the
/// table (PLAN_DEVIATIONS.md D33).
pub(crate) fn top_k(n: usize) -> usize {
    let k = (3.0 * ((n as f64 + 1.0).log2())).ceil() as i64;
    k.clamp(8, 24) as usize
}

pub(crate) const MAX_CANDIDATE_PAIRS: usize = 2000;

/// Character-trigram term-frequency vector for one claim's (lowercased) text.
fn trigram_tf(text: &str) -> BTreeMap<String, u32> {
    let normalized = text.to_lowercase();
    let chars: Vec<char> = normalized.chars().collect();
    let mut tf = BTreeMap::new();
    if chars.len() < 3 {
        return tf;
    }
    for w in chars.windows(3) {
        *tf.entry(w.iter().collect::<String>()).or_insert(0) += 1;
    }
    tf
}

/// Top-K lexical candidate pairs per claim (INTERFACES §3 T1: "normalise ->
/// trigrams -> ... IDF-weighted cosine -> top-K per claim"), deduplicated
/// into an undirected pair list and capped globally at
/// `MAX_CANDIDATE_PAIRS`. SimHash blocking (the spec's own scalability
/// optimization ahead of the cosine step) is not implemented — this computes
/// cosine directly over every pair, which is correct at the claim counts a
/// debate produces before F2's fixture suite exists to stress it, and
/// blocking is purely a performance optimization, never a correctness
/// requirement (PLAN_DEVIATIONS.md D33).
pub(crate) fn top_k_pairs(claims: &[CanonicalClaim]) -> Vec<(usize, usize)> {
    let n = claims.len();
    let tfs: Vec<BTreeMap<String, u32>> = claims.iter().map(|c| trigram_tf(&c.text)).collect();

    let mut df: BTreeMap<&str, u32> = BTreeMap::new();
    for tf in &tfs {
        for term in tf.keys() {
            *df.entry(term.as_str()).or_insert(0) += 1;
        }
    }
    let idf = |term: &str| -> f64 {
        ((1.0 + n as f64) / (1.0 + *df.get(term).unwrap_or(&0) as f64)).ln() + 1.0
    };

    let vectors: Vec<BTreeMap<&str, f64>> = tfs
        .iter()
        .map(|tf| {
            tf.iter()
                .map(|(term, &count)| (term.as_str(), count as f64 * idf(term)))
                .collect()
        })
        .collect();

    let cosine = |a: &BTreeMap<&str, f64>, b: &BTreeMap<&str, f64>| -> f64 {
        let mut dot = 0.0;
        for (term, &wa) in a {
            if let Some(&wb) = b.get(term) {
                dot += wa * wb;
            }
        }
        let norm_a = a.values().map(|v| v * v).sum::<f64>().sqrt();
        let norm_b = b.values().map(|v| v * v).sum::<f64>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            0.0
        } else {
            dot / (norm_a * norm_b)
        }
    };

    let k = top_k(n);
    let mut pair_set: BTreeSet<(usize, usize)> = BTreeSet::new();
    for i in 0..n {
        let mut scored: Vec<(usize, f64)> = (0..n)
            .filter(|&j| j != i)
            .map(|j| (j, cosine(&vectors[i], &vectors[j])))
            .filter(|&(_, score)| score > 0.0)
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for &(j, _) in scored.iter().take(k) {
            let pair = if i < j { (i, j) } else { (j, i) };
            pair_set.insert(pair);
            if pair_set.len() >= MAX_CANDIDATE_PAIRS {
                break;
            }
        }
        if pair_set.len() >= MAX_CANDIDATE_PAIRS {
            break;
        }
    }
    pair_set.into_iter().collect()
}

/// Connected components over a T1/T2 candidate graph, first-fit-decreasing
/// packed into batches of at most `max_batch` items (INTERFACES §3's
/// partition-then-pack step). A single component larger than `max_batch` is
/// simply placed in its own oversized batch rather than split further — a
/// batch that large means the candidate graph already found everything
/// densely interconnected, which the spec's own token-budget concern
/// (truncation) is a real risk for for that batch specifically, but
/// splitting a genuinely connected component would defeat the point of
/// partitioning by it in the first place.
pub(crate) fn partition_into_batches(
    n: usize,
    pairs: &[(usize, usize)],
    max_batch: usize,
) -> Vec<Vec<usize>> {
    if n <= max_batch {
        return vec![(0..n).collect()];
    }

    let mut uf = UnionFind::new(n);
    for &(a, b) in pairs {
        uf.union(a, b);
    }
    let mut components: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        components.entry(uf.find(i)).or_default().push(i);
    }
    let mut components: Vec<Vec<usize>> = components.into_values().collect();
    components.sort_by_key(|b| std::cmp::Reverse(b.len()));

    let mut batches: Vec<Vec<usize>> = Vec::new();
    for component in components {
        if let Some(batch) = batches
            .iter_mut()
            .find(|b| b.len() + component.len() <= max_batch)
        {
            batch.extend(component);
        } else {
            batches.push(component);
        }
    }
    batches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_k_is_clamped_and_monotonic() {
        assert_eq!(top_k(0), 8);
        assert!(top_k(12) >= 8 && top_k(12) <= 24);
        assert_eq!(top_k(10_000), 24);
        assert!(top_k(300) <= top_k(3_000_000));
    }

    #[test]
    fn union_find_merges_transitively() {
        let mut uf = UnionFind::new(4);
        uf.union(0, 1);
        uf.union(1, 2);
        assert_eq!(uf.find(0), uf.find(2));
        assert_ne!(uf.find(0), uf.find(3));
    }

    #[test]
    fn partition_into_batches_keeps_everything_under_the_cap_when_it_fits() {
        let batches = partition_into_batches(10, &[], 60);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 10);
    }

    #[test]
    fn partition_into_batches_packs_connected_components_together() {
        let pairs = vec![(0, 1), (1, 2)];
        let batches = partition_into_batches(5, &pairs, 2);
        let component_batch = batches.iter().find(|b| b.contains(&0)).unwrap();
        assert!(component_batch.contains(&1) && component_batch.contains(&2));
    }
}
