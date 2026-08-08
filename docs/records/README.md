# Development-history records

This repository's public history starts at the published tree. The
development history behind it is private, but the posts and the README cite
specific commits from it by hash. Each file here is that commit's message,
quoted verbatim (`git show -s <hash>`), so every citation resolves to the
record it names. Measurements inside these records describe the tree as it
stood at that commit and are labeled with their dates; the README's
Performance section documents which of them still reproduce on the current
tree and which have drifted.

| Record | What it fixed or measured |
|---|---|
| `e4572d6.md` | Deletion tracking: the incomplete-rename blind spot (sensitivity) |
| `833c17f.md` | Stale scan mirrors the resolver's own matching rule: 1,015 false positives to 0 (specificity) |
| `2cf8aeb.md` | Batched node/edge writes: the index throughput fix |
