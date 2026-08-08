//! Typed-hypergraph canonicalizer: individualization-refinement (I-R) with
//! edge labels and seeded vertex colors.
//!
//! PROVENANCE: ported from the bend-multiway spike's ground-truth rewriter,
//! `/Users/mike/dev/bend-multiway-spike/rust/src/lib.rs` (same-author code
//! from this portfolio; read once at port time, 2026-07-21). The spike proved
//! `incidence`/`signature`/`refine`/`certificate`/`transposition_is_auto`/
//! `canon_search`/`canonical` against an O(n!) brute-force oracle over a
//! random-hypergraph corpus; this port keeps that differential-test
//! methodology (see `canon_tests` below) and extends the algorithm for code
//! graphs:
//!
//! * TYPED edges — every hyperedge carries a static [`Label`] (the edge
//!   type: calls / contains / implements / …). The label enters the
//!   refinement signature and the certificate ordering. Labels are symbols,
//!   not vertex ids, so relabel-invariance is preserved.
//! * SEEDED vertex colors — the initial partition groups vertices by a
//!   caller-supplied [`Color`] (node kind) instead of uniform zero. I-R
//!   accepts any initial partition; seeding both speeds refinement (fewer
//!   rounds to discretize) and bakes kind into identity: "a function calling
//!   a struct" is not "a struct containing a function".
//! * A deterministic, persistence-grade [`certificate_hash`] — hand-rolled
//!   FNV-1a over a fixed canonical serialization, NOT `DefaultHasher`
//!   (whose per-process random seed makes hashes useless to store).
//!
//! The algorithm, unchanged from the spike:
//!   1. color-refinement (1-WL): colour vertices by an iso-invariant
//!      signature and refine to a stable partition. For structured/sparse
//!      graphs this discretizes (all cells singletons) with NO search.
//!   2. if a non-singleton cell remains (symmetry), individualize each of
//!      its orbit representatives in turn, re-refine, recurse, and take the
//!      MIN certificate. Min-over-all-leaves makes it a *complete*
//!      canonicalizer; exponential only on highly symmetric inputs.

use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Caller-facing vertex id. Arbitrary (non-contiguous ids are fine);
/// [`canonical`] anonymizes them, which is the entire point.
pub type Vertex = u32;

/// Static edge-type label. Callers derive it from a stable symbol (e.g.
/// [`fnv1a64`] of the edge-type string) so certificates compare across
/// graphs and across processes.
pub type Label = u64;

/// Seed vertex color, as an ordered pair so a caller can layer an
/// individualization class over a kind with zero collision risk: `(0, kind)`
/// sorts strictly before every `(1, kind)` — used by the fingerprint layer
/// to root a certificate at its center vertex.
pub type Color = (u64, u64);

/// One typed hyperedge: a static label plus an ORDERED vertex list (order
/// encodes direction/position, exactly as in the spike's ordered edges).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TypedEdge {
    pub label: Label,
    pub verts: Vec<Vertex>,
}

/// A typed, vertex-colored hypergraph. Every vertex appearing in `edges`
/// MUST have an entry in `colors`; entries with no incident edge are legal
/// (isolated vertices participate in the certificate).
#[derive(Debug, Clone, Default)]
pub struct TypedGraph {
    pub edges: Vec<TypedEdge>,
    pub colors: BTreeMap<Vertex, Color>,
}

/// Canonical form. Two graphs get the IDENTICAL certificate iff they are
/// isomorphic as typed colored hypergraphs (a vertex bijection preserving
/// edge labels, vertex order within edges, and seed colors).
///
/// `colors[i]` is the seed color of canonical vertex `i`: refinement only
/// ever splits seed classes and class ranking preserves seed order, so this
/// is always the sorted seed multiset — an invariant header that makes
/// graphs with equal structure but different kinds distinct.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Certificate {
    pub colors: Vec<Color>,
    pub edges: Vec<(Label, Vec<u32>)>,
}

// ---- refinement core --------------------------------------------------------

/// A vertex's refinement signature: for every incident edge, its label, its
/// arity, the edge's colour vector, and the positions this vertex occupies.
/// Uses only labels + colours + structure (never vertex ids), so it is
/// relabelling-invariant.
type Sig = Vec<(Label, usize, Vec<usize>, Vec<usize>)>;

/// vertex -> indices of the edges incident to it (each incident edge listed
/// once, even when the vertex occupies several positions in it — matching
/// `signature`'s one-entry-per-edge shape). Depends only on structure, so it
/// is built once per graph and reused across every `refine` in the search.
fn incidence(n: usize, edges: &[(Label, Vec<usize>)]) -> Vec<Vec<usize>> {
    let mut inc = vec![Vec::new(); n];
    for (ei, (_, e)) in edges.iter().enumerate() {
        let mut distinct = e.clone();
        distinct.sort_unstable();
        distinct.dedup();
        for u in distinct {
            inc[u].push(ei);
        }
    }
    inc
}

/// `inc_v` = the edge indices incident to `v`. Only those can contribute to
/// v's signature, so scanning them (instead of every edge) keeps a
/// refinement round at O(total incidence) — the spike's speedup, unchanged.
fn signature(
    v: usize,
    colors: &[usize],
    edges: &[(Label, Vec<usize>)],
    inc_v: &[usize],
) -> Sig {
    let mut sig = Sig::with_capacity(inc_v.len());
    for &ei in inc_v {
        let (label, e) = &edges[ei];
        let pos: Vec<usize> =
            e.iter().enumerate().filter(|(_, &u)| u == v).map(|(i, _)| i).collect();
        sig.push((*label, e.len(), e.iter().map(|&u| colors[u]).collect(), pos));
    }
    sig.sort();
    sig
}

fn distinct_count(colors: &[usize]) -> usize {
    colors.iter().copied().collect::<BTreeSet<_>>().len()
}

/// Refine a colouring to a stable (equitable) partition. New classes are
/// ranked by (old colour, signature), so refinement only ever SPLITS the
/// initial partition and preserves its order — the property that makes the
/// certificate's sorted-seed-colors header exact.
///
/// ALWAYS returns dense ranks `0..#classes`, including when the input is
/// already stable. Everything downstream is order-based (cell grouping,
/// `certificate_edges`' rank map), so the renormalization is semantically
/// neutral — but it is load-bearing for termination: [`canon_search`]
/// individualizes by doubling every colour, and doubling COMPOUNDS when a
/// stable partition round-trips unrenormalized. Colour values then grow as
/// 2^depth, silently wrap `usize` at depth 64 (release-mode unchecked
/// arithmetic), and the wrapped values collide — merging cells and undoing
/// earlier individualizations, so the search re-individualizes forever.
/// That non-termination is what actually aborted `index` on real repos
/// (`specs/receipts/pub-prep-verify-20260730.md` §3.4 — the recursive
/// search turned it into a stack overflow; an iterative search turns it
/// into a hang, which is how it was isolated).
fn refine(init: &[usize], edges: &[(Label, Vec<usize>)], inc: &[Vec<usize>]) -> Vec<usize> {
    let n = init.len();
    let mut colors = init.to_vec();
    loop {
        let keyed: Vec<(usize, Sig)> =
            (0..n).map(|v| (colors[v], signature(v, &colors, edges, &inc[v]))).collect();
        let mut classes: Vec<(usize, Sig)> = keyed.clone();
        classes.sort();
        classes.dedup();
        let stable = classes.len() == distinct_count(&colors);
        colors = keyed.iter().map(|k| classes.binary_search(k).unwrap()).collect();
        if stable {
            return colors;
        }
    }
}

/// Discrete colouring -> canonical relabelled+sorted labeled edge list (the
/// edges half of the certificate; the colors header is leaf-invariant and
/// added by [`canonical`]).
fn certificate_edges(colors: &[usize], edges: &[(Label, Vec<usize>)]) -> Vec<(Label, Vec<u32>)> {
    let mut sorted = colors.to_vec();
    sorted.sort();
    let rank: HashMap<usize, usize> = sorted.iter().enumerate().map(|(r, &c)| (c, r)).collect();
    let mut out: Vec<(Label, Vec<u32>)> = edges
        .iter()
        .map(|(l, e)| (*l, e.iter().map(|&u| rank[&colors[u]] as u32).collect()))
        .collect();
    out.sort();
    out
}

/// Is the transposition `(a b)` an automorphism of the LABELED graph? Called
/// only for `a`,`b` in the SAME colour cell — refined cells never straddle
/// seed classes, so equal colour already makes the swap colour-preserving;
/// only labeled edge-structure invariance is left to check. A `true` result
/// proves `a` and `b` share an orbit, hence their individualization branches
/// yield the identical certificate (one suffices).
fn transposition_is_auto(a: usize, b: usize, edges: &[(Label, Vec<usize>)]) -> bool {
    let swap = |u: usize| {
        if u == a {
            b
        } else if u == b {
            a
        } else {
            u
        }
    };
    let mut swapped: Vec<(Label, Vec<usize>)> = edges
        .iter()
        .map(|(l, e)| (*l, e.iter().map(|&u| swap(u)).collect()))
        .collect();
    let mut orig: Vec<(Label, Vec<usize>)> = edges.to_vec();
    orig.sort();
    swapped.sort();
    orig == swapped
}

/// I-R search: min certificate over every discrete leaf, with orbit pruning
/// (one representative per automorphism orbit inside a cell — collapses the
/// k! blow-up on symmetric graphs; a fingerprint neighborhood's k
/// interchangeable callers are exactly this case).
///
/// Iterative on an explicit heap stack, deliberately. The search
/// individualizes ONE vertex per level, so its depth is the size of the
/// largest cell refinement can't split — and orbit pruning collapses
/// *breadth*, never depth. Depth is only bounded at all because [`refine`]
/// renormalizes colours to dense ranks (see its doc comment for the wrap
/// bug that otherwise un-individualizes vertices and makes the chain
/// unbounded — the actual `index` abort on real repos, receipt §3.4). With
/// that bound in place, native frames are still the wrong place to spend
/// it: a hot symbol in a large repo accumulates thousands of structurally
/// interchangeable callers in one cell, and one native frame per
/// individualization is a stack overflow waiting for a bigger repo.
/// Min-over-leaves is order-independent, so a single running `best` over a
/// DFS reproduces the recursion's certificate exactly.
fn canon_search(
    colors: &[usize],
    edges: &[(Label, Vec<usize>)],
    inc: &[Vec<usize>],
) -> Vec<(Label, Vec<u32>)> {
    /// One suspended search node: its refined coloring, its orbit
    /// representatives, and how many have been descended into so far.
    struct Frame {
        colors: Vec<usize>,
        reps: Vec<usize>,
        next: usize,
    }

    let mut best: Option<Vec<(Label, Vec<u32>)>> = None;
    let mut stack: Vec<Frame> = Vec::new();
    // The node about to be examined; None means "advance the top frame".
    let mut cur: Option<Vec<usize>> = Some(colors.to_vec());

    loop {
        if let Some(node_colors) = cur.take() {
            let mut cells: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
            for (v, &c) in node_colors.iter().enumerate() {
                cells.entry(c).or_default().push(v);
            }
            match cells.values().find(|vs| vs.len() > 1) {
                None => {
                    let cand = certificate_edges(&node_colors, edges);
                    if best.as_ref().is_none_or(|b| cand < *b) {
                        best = Some(cand);
                    }
                }
                Some(cell) => {
                    let mut reps: Vec<usize> = Vec::new();
                    for &v in cell {
                        if !reps.iter().any(|&r| transposition_is_auto(r, v, edges)) {
                            reps.push(v);
                        }
                    }
                    stack.push(Frame { colors: node_colors, reps, next: 0 });
                }
            }
        } else {
            match stack.last_mut() {
                None => break,
                Some(top) if top.next < top.reps.len() => {
                    let v = top.reps[top.next];
                    top.next += 1;
                    // individualize v: split it out of its cell, order
                    // otherwise stable
                    let mut indiv: Vec<usize> = top.colors.iter().map(|&c| c * 2).collect();
                    indiv[v] += 1;
                    cur = Some(refine(&indiv, edges, inc));
                }
                Some(_) => {
                    stack.pop();
                }
            }
        }
    }
    best.expect("the search reaches at least one discrete leaf")
}

// ---- public entry points ----------------------------------------------------

/// Canonical form via individualization-refinement. Isomorphic typed colored
/// hypergraphs map to the identical certificate; scales to large structured
/// graphs (unlike the oracle).
///
/// Panics if an edge references a vertex missing from `colors` — that is a
/// caller bug (the color map defines the vertex universe).
pub fn canonical(g: &TypedGraph) -> Certificate {
    for e in &g.edges {
        for v in &e.verts {
            assert!(
                g.colors.contains_key(v),
                "canon: edge vertex {v} has no seed color — colors must cover every endpoint"
            );
        }
    }
    let verts: Vec<Vertex> = g.colors.keys().copied().collect();
    let n = verts.len();
    if n == 0 {
        return Certificate::default();
    }
    let index: HashMap<Vertex, usize> = verts.iter().enumerate().map(|(i, &v)| (v, i)).collect();
    let edges: Vec<(Label, Vec<usize>)> = g
        .edges
        .iter()
        .map(|e| (e.label, e.verts.iter().map(|v| index[v]).collect()))
        .collect();
    let seed: Vec<Color> = verts.iter().map(|v| g.colors[v]).collect();

    // Dense initial partition: rank of each seed color among the sorted
    // distinct seed colors. Arbitrary u64 pairs in, small usizes out —
    // ordering (all `refine` cares about) is preserved exactly.
    let mut distinct: Vec<Color> = seed.clone();
    distinct.sort_unstable();
    distinct.dedup();
    let init: Vec<usize> =
        seed.iter().map(|c| distinct.binary_search(c).expect("own color")).collect();

    let inc = incidence(n, &edges);
    let canon = canon_search(&refine(&init, &edges, &inc), &edges, &inc);

    let mut colors_sorted = seed;
    colors_sorted.sort_unstable();
    Certificate { colors: colors_sorted, edges: canon }
}

/// FNV-1a (64-bit) — the standard offset-basis/prime constants, hand-rolled
/// so hashes are stable across processes and machine restarts (never
/// `DefaultHasher`/`RandomState`, whose per-process seed would make persisted
/// hashes garbage). Also used by callers to derive stable [`Label`]s and
/// [`Color`] kinds from strings.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Streaming FNV-1a over the certificate's fixed canonical serialization:
/// `colors.len() ‖ (c.0, c.1)* ‖ edges.len() ‖ (label ‖ arity ‖ verts…)*`,
/// every integer little-endian (u64 except the u32 vertex ranks). Length
/// prefixes make the encoding prefix-free, so distinct certificates never
/// collide by concatenation ambiguity.
pub fn certificate_hash(cert: &Certificate) -> u64 {
    struct Fnv(u64);
    impl Fnv {
        fn eat(&mut self, bytes: &[u8]) {
            for &b in bytes {
                self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        fn u64(&mut self, v: u64) {
            self.eat(&v.to_le_bytes());
        }
        fn u32(&mut self, v: u32) {
            self.eat(&v.to_le_bytes());
        }
    }
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    h.u64(cert.colors.len() as u64);
    for &(class, kind) in &cert.colors {
        h.u64(class);
        h.u64(kind);
    }
    h.u64(cert.edges.len() as u64);
    for (label, verts) in &cert.edges {
        h.u64(*label);
        h.u64(verts.len() as u64);
        for &v in verts {
            h.u32(v);
        }
    }
    h.0
}

// ---- brute-force oracle (test-only) ----------------------------------------

/// Exhaustive O(n!) canonical form — the correct-by-construction oracle the
/// I-R canonicaliser is differential-tested against. Permutes vertices only;
/// labels and seed colors ride along fixed. The candidate compared is the
/// full `(permuted color vector, permuted+sorted edges)` pair, so the min
/// lands on the sorted color vector and, among the permutations achieving
/// it, the least edge list — the same (colors, edges) shape [`canonical`]
/// emits. Only sane to ~8 vertices; never for production.
#[cfg(test)]
fn canonical_bruteforce(g: &TypedGraph) -> Certificate {
    fn permutations(items: Vec<usize>) -> Vec<Vec<usize>> {
        if items.len() <= 1 {
            return vec![items];
        }
        let mut out = Vec::new();
        for i in 0..items.len() {
            let mut rest = items.clone();
            let x = rest.remove(i);
            for mut p in permutations(rest) {
                p.insert(0, x);
                out.push(p);
            }
        }
        out
    }

    let verts: Vec<Vertex> = g.colors.keys().copied().collect();
    let n = verts.len();
    if n == 0 {
        return Certificate::default();
    }
    let index: HashMap<Vertex, usize> = verts.iter().enumerate().map(|(i, &v)| (v, i)).collect();
    let seed: Vec<Color> = verts.iter().map(|v| g.colors[v]).collect();

    type Candidate = (Vec<Color>, Vec<(Label, Vec<u32>)>);
    let mut best: Option<Candidate> = None;
    for perm in permutations((0..n).collect()) {
        // perm[i] = the canonical id assigned to dense vertex i.
        let mut colors_p = vec![(0u64, 0u64); n];
        for (i, &p) in perm.iter().enumerate() {
            colors_p[p] = seed[i];
        }
        let mut edges_p: Vec<(Label, Vec<u32>)> = g
            .edges
            .iter()
            .map(|e| (e.label, e.verts.iter().map(|v| perm[index[v]] as u32).collect()))
            .collect();
        edges_p.sort();
        let cand = (colors_p, edges_p);
        if best.as_ref().is_none_or(|b| cand < *b) {
            best = Some(cand);
        }
    }
    let (colors, edges) = best.expect("n >= 1");
    Certificate { colors, edges }
}

#[cfg(test)]
mod canon_tests {
    //! Differential validation against the O(n!) oracle, in the spike's
    //! style: deterministic LCG corpus (no new deps), relabel-invariance,
    //! all-pairs oracle agreement — extended with label/color sensitivity,
    //! rooting, and cross-process hash stability.
    use super::*;

    /// Tiny deterministic PRNG (no dep) for test corpora — the spike's LCG.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, bound: u32) -> u32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) as u32) % bound
        }
    }

    const LABELS: [Label; 3] = [11, 22, 33];
    const KINDS: [u64; 3] = [7, 8, 9];

    /// Random TYPED hypergraph, ≤8 vertices, random seed colors, arity 2–3
    /// (genuine hyperedges), random labels.
    fn random_graph(r: &mut Lcg) -> TypedGraph {
        let n = 3 + r.next(6); // 3..=8 vertices
        let m = 2 + r.next(4); // 2..=5 edges
        let colors: BTreeMap<Vertex, Color> =
            (0..n).map(|v| (v, (1, KINDS[r.next(3) as usize]))).collect();
        let edges = (0..m)
            .map(|_| {
                let arity = 2 + r.next(2);
                TypedEdge {
                    label: LABELS[r.next(3) as usize],
                    verts: (0..arity).map(|_| r.next(n)).collect(),
                }
            })
            .collect();
        TypedGraph { edges, colors }
    }

    /// Relabel to fresh vertex ids under a random permutation — an
    /// isomorphic copy; colors ride along to the renamed vertices.
    fn relabel(g: &TypedGraph, r: &mut Lcg) -> TypedGraph {
        let verts: Vec<Vertex> = g.colors.keys().copied().collect();
        let n = verts.len();
        let mut perm: Vec<Vertex> = (0..n as Vertex).map(|x| x + 1000).collect();
        for i in (1..n).rev() {
            perm.swap(i, r.next((i + 1) as u32) as usize);
        }
        let map: HashMap<Vertex, Vertex> =
            verts.iter().enumerate().map(|(i, &v)| (v, perm[i])).collect();
        TypedGraph {
            edges: g
                .edges
                .iter()
                .map(|e| TypedEdge {
                    label: e.label,
                    verts: e.verts.iter().map(|v| map[v]).collect(),
                })
                .collect(),
            colors: g.colors.iter().map(|(v, &c)| (map[v], c)).collect(),
        }
    }

    #[test]
    fn ir_is_relabel_invariant() {
        let mut r = Lcg(0xC0FFEE);
        for _ in 0..3000 {
            let g = random_graph(&mut r);
            let h = relabel(&g, &mut r);
            assert_eq!(canonical(&g), canonical(&h), "not relabel-invariant:\n{g:?}\n{h:?}");
        }
    }

    #[test]
    fn ir_partitions_identically_to_bruteforce() {
        // Gold standard: I-R and the exhaustive oracle must agree on
        // isomorphism for every pair in a random corpus (same canonical <=>
        // same iso class), labels and seed colors included.
        let mut r = Lcg(0x1234_5678);
        let batch: Vec<TypedGraph> = (0..150).map(|_| random_graph(&mut r)).collect();
        let ir: Vec<Certificate> = batch.iter().map(canonical).collect();
        let bf: Vec<Certificate> = batch.iter().map(canonical_bruteforce).collect();
        for i in 0..batch.len() {
            for j in 0..batch.len() {
                assert_eq!(
                    ir[i] == ir[j],
                    bf[i] == bf[j],
                    "I-R vs brute-force disagree on iso of:\n{:?}\n{:?}",
                    batch[i],
                    batch[j]
                );
            }
        }
    }

    fn two_path(l1: Label, l2: Label, c: [Color; 3]) -> TypedGraph {
        TypedGraph {
            edges: vec![
                TypedEdge { label: l1, verts: vec![0, 1] },
                TypedEdge { label: l2, verts: vec![1, 2] },
            ],
            colors: [(0, c[0]), (1, c[1]), (2, c[2])].into_iter().collect(),
        }
    }

    #[test]
    fn labels_are_part_of_identity() {
        let same = (1, 7);
        let a = two_path(11, 11, [same, same, same]);
        let b = two_path(11, 22, [same, same, same]);
        assert_ne!(canonical(&a), canonical(&b), "edge labels must distinguish certificates");
    }

    #[test]
    fn seed_colors_are_part_of_identity() {
        let a = two_path(11, 11, [(1, 7), (1, 7), (1, 7)]);
        let b = two_path(11, 11, [(1, 8), (1, 7), (1, 7)]);
        assert_ne!(canonical(&a), canonical(&b), "vertex kinds must distinguish certificates");
    }

    #[test]
    fn center_rooting_distinguishes_position() {
        // 0→1←2 (two same-label edges converging on the middle): rooting an
        // END vs the MIDDLE must differ; the two ends are genuinely
        // symmetric here (unlike a directed path), so rooting either end
        // must agree.
        let converge = |c: [Color; 3]| TypedGraph {
            edges: vec![
                TypedEdge { label: 11, verts: vec![0, 1] },
                TypedEdge { label: 11, verts: vec![2, 1] },
            ],
            colors: [(0, c[0]), (1, c[1]), (2, c[2])].into_iter().collect(),
        };
        let end = converge([(0, 7), (1, 7), (1, 7)]);
        let mid = converge([(1, 7), (0, 7), (1, 7)]);
        assert_ne!(canonical(&end), canonical(&mid), "rooting must be positional");
        let other_end = converge([(1, 7), (1, 7), (0, 7)]);
        assert_eq!(canonical(&end), canonical(&other_end));
    }

    /// Cross-process hash stability: this constant was computed once by this
    /// very function and pasted in. `DefaultHasher` would fail this test on
    /// every fresh process; FNV-1a over the fixed serialization cannot.
    #[test]
    fn certificate_hash_is_stable_across_processes() {
        let g = TypedGraph {
            edges: vec![
                TypedEdge { label: 11, verts: vec![10, 20] },
                TypedEdge { label: 22, verts: vec![20, 30, 40] },
                TypedEdge { label: 11, verts: vec![40, 10] },
            ],
            colors: [(10, (0, 7)), (20, (1, 8)), (30, (1, 9)), (40, (1, 7))]
                .into_iter()
                .collect(),
        };
        let h = certificate_hash(&canonical(&g));
        assert_eq!(
            h, 1_946_827_007_203_589_284_u64,
            "pinned fixture hash drifted — the canonical serialization (or the \
             algorithm) changed; persisted fingerprints would be invalidated"
        );
    }

    #[test]
    fn ir_scales_far_past_the_bruteforce_limit() {
        // Typed directed path of 40 vertices: 40! is hopeless for the
        // oracle, but colour refinement discretizes it. Completing IS the
        // perf proof (the spike's test, typed).
        let path = |base: Vertex| TypedGraph {
            edges: (0..40).map(|i| TypedEdge { label: 11, verts: vec![base + i, base + i + 1] }).collect(),
            colors: (0..=40).map(|i| (base + i, (1, 7))).collect(),
        };
        let mut r = Lcg(9);
        let g = path(0);
        assert_eq!(canonical(&g), canonical(&relabel(&g, &mut r)), "40-vertex path invariance");
        assert_eq!(canonical(&g).edges.len(), 40);
    }

    #[test]
    fn star_with_many_interchangeable_leaves_is_fast_via_orbit_pruning() {
        // The production shape: a fingerprint neighborhood is a star around
        // its individualized center. 64 same-kind callers = one orbit; the
        // k! blow-up must collapse via transposition pruning. Completing
        // (quickly) is the point of the assertion.
        let star = TypedGraph {
            edges: (1..=64).map(|i| TypedEdge { label: 11, verts: vec![i, 0] }).collect(),
            colors: std::iter::once((0, (0u64, 7u64)))
                .chain((1..=64).map(|i| (i, (1, 8))))
                .collect(),
        };
        let mut r = Lcg(0xBEEF);
        assert_eq!(canonical(&star), canonical(&relabel(&star, &mut r)));
        assert_eq!(canonical(&star).edges.len(), 64);
    }

    /// Regression for the `index` abort above ~700–750 Rust files
    /// (`specs/receipts/pub-prep-verify-20260730.md` §3.4), which was two
    /// stacked defects: colour-doubling compounded across levels until it
    /// wrapped `usize` at depth 64, un-individualizing earlier vertices so
    /// the search never terminated (fixed by `refine` renormalizing), and
    /// the recursive search spent one native frame per individualization,
    /// which turned that non-termination into a stack-overflow abort (fixed
    /// by the explicit-stack rewrite). A cell of same-colored isolated
    /// vertices is the cheapest shape that forces a deep individualization
    /// chain — a star of interchangeable callers is the production one, but
    /// its orbit checks re-sort every edge per candidate, a separate queued
    /// perf item. The 512 KiB thread makes the native-stack bound explicit:
    /// the pre-fix implementation aborts this exact test; the fixed one
    /// must complete at a depth (3,000) far past the wrap threshold.
    #[test]
    fn deep_individualization_chain_completes_on_a_small_stack() {
        let k: Vertex = 3_000;
        let g = TypedGraph {
            edges: vec![],
            colors: (0..k).map(|v| (v, (1, 7))).collect(),
        };
        let cert = std::thread::Builder::new()
            .stack_size(512 * 1024)
            .spawn(move || canonical(&g))
            .expect("spawn")
            .join()
            .expect("canonical must not overflow the native stack");
        assert_eq!(cert.colors.len(), k as usize);
        assert!(cert.edges.is_empty());
    }
}
