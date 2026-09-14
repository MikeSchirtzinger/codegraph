# Verbatim development-history record

Quoted from the private development history with `git show -s d95bad6`.

```
commit d95bad6047ac5578d211fc871e37ea3fc415c30a
Author: Mike Schirtzinger <155995654+MikeSchirtzinger@users.noreply.github.com>
Date:   Mon Sep 14 04:16:56 2026 -0400

    fix(plan): a file that exists binds, indexed or not
    
    The dogfood found 23 of 61 touches in this repo's roadmap UNRESOLVED, every
    one of them a spec, a README, or src/schema.surql: present, correct, and
    outside the file types codegraph has a grammar for. `plan stale` therefore
    reported 14 items on a roadmap with nothing wrong with it, which is not a
    staleness signal.
    
    UNRESOLVED was carrying two facts. "The plan names something that is not
    there" is a stale plan; "the code graph has no node for this" is not. A
    `file:` touch whose path exists in the working tree now binds RESOLVED to
    that path with a new boolean `indexed`, and `glob:` expands against the
    union of the indexed paths and the working tree, one row per match, each
    with its own flag. Confidence keeps its exact meaning and the second fact
    gets its own column.
    
    The reason vocabulary shrank to match: no_such_file, no_such_symbol,
    no_glob_match, ambiguous_candidates. Nothing produces no_such_file_in_index
    or file_type_not_indexed any more, and dead vocabulary in a machine-readable
    field is worse than none.
    
    The working tree list is found the way the indexer finds its own, git
    ls-files first and a directory walk with the same skip list as the fallback.
    index::discover_source_files could not be reused: it filters to languages
    with grammars, which is the filter that has to be lifted here.
    
    `stale` now flags only paths that do not exist, symbols that did not bind,
    and globs that matched nothing. `touching` and `collisions` are unchanged on
    unindexed paths, since the target is the path either way. `blast` skips an
    unindexed target, which has no node to walk from, and reports how many
    rather than dropping them: a reach over fewer seeds is a lower bound.
    `show` and `list` mark an unindexed touch "(not indexed)".
    
    Evidence. Same store, same index, same unmodified planes.yaml: 61 selectors
    now 61 resolved, 0 ambiguous, 0 unresolved, down from 38/0/23; 303 touch
    rows of which 105 bind to a path the graph holds nothing for; `plan stale`
    reports 0 items, down from 14. The row count rose from 221 because a glob
    now sees the working tree it always meant. Measured in an isolated worktree
    at this tree: 0 warnings, 337 passed 0 failed over three consecutive runs.
    tests/plan_ops.rs stays at 10, with a_real_but_unindexed_path_binds_and_is_
    never_stale pinning every new behavior, including that re-indexing flips
    `indexed` without changing the confidence. Receipt section 9.
```
