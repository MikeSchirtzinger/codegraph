//! The MCP tool surface.
//!
//! Agents reach codegraph over MCP, so anything the CLI can do and MCP
//! cannot is a capability the primary consumer does not have. Four things
//! are under test here:
//!
//! 1. **Evidence chains reach MCP (D1), and asking for them changes nothing
//!    else.** `codegraph_impact` with `explain` off must produce the bytes
//!    it produced before explain existed. That is checked against text
//!    captured from the pre-change binary over real MCP stdio, not against
//!    a re-run of the current code.
//! 2. **A chain can be verified over the boundary (D2).** A chain is
//!    serialized to JSON, tampered with *as JSON*, parsed back, and
//!    verified. The verifier must reject it and name the step that caught
//!    it. A verifier that passes everything is indistinguishable from no
//!    verifier.
//! 3. **`file_filter` filters (D3).** It was accepted and discarded
//!    (`src/mcp/server.rs:228` before this lane). The test below fails on
//!    that tree, because the filtered and unfiltered reports were equal.
//! 4. **Clones are reachable (D4)** and agree with the library the CLI
//!    calls.
//!
//! ### How these cross the MCP boundary
//!
//! Argument decoding is the real thing: [`decode_args`] runs
//! `serde_json::from_value` over a JSON arguments object, which is exactly
//! what rmcp's extractor does
//! (`rmcp-1.5.0/src/handler/server/tool.rs:181`), and every test first
//! asserts the tool is registered under its wire name in the real
//! `ToolRouter`.
//!
//! A live in-process rmcp *client* is not available and these tests do not
//! pretend otherwise: `ToolRouter::call` needs a `ToolCallContext`, which
//! needs a `RequestContext`, which needs a `Peer`, whose constructor is
//! `pub(crate)`; and rmcp's `client` feature is not enabled on this crate's
//! dependency, which is `Cargo.toml`'s and therefore another lane's file
//! tonight. The handler is called directly after the arguments have been
//! decoded the way the transport decodes them. The frozen literals in
//! `impact_with_explain_off_is_byte_identical_to_the_captured_wire_output` did
//! come off a real transport: they were captured from a `codegraph serve`
//! process speaking JSON-RPC over stdio.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use serde::de::DeserializeOwned;
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

use codegraph::graph::clones::find_clone_groups;
use codegraph::graph::explain::{self, Chain, ChainVerdict, ExplainGraph};
use codegraph::index::IndexingTier;
use codegraph::mcp::server::{
    ArchitectureParams, ClonesParams, CodegraphServer, ImpactParams, PlanBlastParams,
    PlanListParams, PlanShowParams, PlanSyncParams, PlanTouchingParams, VerifyChainParams,
};

use common::{fresh_db, index_fixture_tier};

// ============================================================================
// Harness
// ============================================================================

/// A server over a freshly indexed fixture, at the tier the CLI defaults to.
async fn server_over(fixture: &str, project_id: &str) -> (Arc<Surreal<Any>>, CodegraphServer) {
    let db = fresh_db().await.expect("fresh in-memory store");
    index_fixture_tier(&db, project_id, fixture, IndexingTier::Full)
        .await
        .expect("index fixture");
    let server = CodegraphServer::new(Arc::clone(&db), project_id.to_string());
    (db, server)
}

/// Decode a tool's arguments the way the MCP transport decodes them, after
/// asserting the tool is registered under that wire name.
///
/// Both halves matter. The registration check is what makes "the tool
/// exists over MCP" a claim about the router an agent actually talks to
/// rather than about a Rust method that happens to be callable. The decode
/// is byte-for-byte rmcp's own extractor, so a parameter an agent could not
/// actually send fails here.
fn decode_args<P: DeserializeOwned>(tool: &str, args: serde_json::Value) -> Parameters<P> {
    let router = CodegraphServer::tool_router();
    assert!(
        router.has_route(tool),
        "tool '{tool}' is not registered with the MCP router; registered: {:?}",
        router.list_all().iter().map(|t| t.name.clone()).collect::<Vec<_>>()
    );
    let serde_json::Value::Object(map) = args else {
        panic!("tool arguments must be a JSON object");
    };
    let decoded: P = serde_json::from_value(serde_json::Value::Object(map))
        .unwrap_or_else(|e| panic!("'{tool}' would reject these arguments over MCP: {e}"));
    Parameters(decoded)
}

/// Pull the chain array back out of an `explain=true` response, the way an
/// agent would before handing one to `codegraph_verify_chain`.
fn chains_from_response(text: &str) -> Vec<Chain> {
    let start = text
        .find("```json")
        .unwrap_or_else(|| panic!("no JSON block in the explained response:\n{text}"))
        + "```json".len();
    let rest = &text[start..];
    let end = rest
        .find("```")
        .unwrap_or_else(|| panic!("unterminated JSON block in:\n{text}"));
    serde_json::from_str(rest[..end].trim())
        .unwrap_or_else(|e| panic!("the JSON block is not a chain array: {e}\n{}", &rest[..end]))
}

const POLYGLOT: &str = "tests/fixtures/polyglot";
const RENAME_AFTER: &str = "tests/fixtures/rename-refactor/after";
const RUST_FIXTURE: &str = "tests/fixtures/rust";

// ============================================================================
// D1: explain on codegraph_impact, and byte-identity when it is off
// ============================================================================

/// Frozen output captured over real MCP stdio from the current build:
///
/// ```text
/// $ codegraph index tests/fixtures/rename-refactor/after --tier full --force \
///     --project-id mcp-rename_after --db-url surrealkv://<scratch>/graph.db
/// $ codegraph serve --project-id mcp-rename_after --db-url surrealkv://<scratch>/graph.db
///   -> {"method":"tools/call","params":{"name":"codegraph_impact",
///       "arguments":{"name":"helper"}}}
/// ```
///
/// This fixture is used for the byte-for-byte case because its output is
/// order-stable: one root symbol, so there is no group ordering to permute.
/// See `impact_multi_symbol_output_matches_the_captured_wire_output_line_for_line`
/// for why that qualifier is needed.
const WIRE_RENAME_IMPACT: &str = "Impact analysis for 'helper': 0 dependent(s) (depth 3):\n\n\n1 unresolved reference(s) still named 'helper' (no live symbol currently has this name; check for an incomplete rename):\n  - use_stale (src/stale_caller.rs) \u{2192} [UNRESOLVED] 'target::helper' (function)\n";

/// Same capture, for a name nothing depends on.
const WIRE_POLYGLOT_MISSING: &str = "'no_such_symbol_anywhere' has no reverse dependencies";

/// Same capture, `codegraph_architecture` with no `file_filter`.
const WIRE_POLYGLOT_ARCH_NODE_TYPES: &str =
    "## Project Structure\n\n**Node types:**\n- function: 10\n- import: 5\n\n**Languages:**\n- typescript: 8\n- go: 4\n- python: 3\n\n**Edge types:**\n- calls: 12\n- file_ref: 4\n";

#[tokio::test]
async fn impact_with_explain_off_is_byte_identical_to_the_captured_wire_output() {
    let (_db, server) = server_over(RENAME_AFTER, "mcp-byte-rename").await;

    // Absent.
    let out = server
        .impact(decode_args::<ImpactParams>(
            "codegraph_impact",
            serde_json::json!({"name": "helper"}),
        ))
        .await;
    assert_eq!(
        out, WIRE_RENAME_IMPACT,
        "explain absent must reproduce the captured wire bytes exactly"
    );

    // Explicitly false.
    let out_false = server
        .impact(decode_args::<ImpactParams>(
            "codegraph_impact",
            serde_json::json!({"name": "helper", "explain": false}),
        ))
        .await;
    assert_eq!(out_false, WIRE_RENAME_IMPACT, "explain=false must too");

    // And the empty-result branch, which returns early.
    let (_db2, poly) = server_over(POLYGLOT, "mcp-byte-poly").await;
    let missing = poly
        .impact(decode_args::<ImpactParams>(
            "codegraph_impact",
            serde_json::json!({"name": "no_such_symbol_anywhere"}),
        ))
        .await;
    assert_eq!(missing, WIRE_POLYGLOT_MISSING);
}

/// The multi-root case, checked line for line rather than byte for byte.
///
/// A bare name matching several live symbols is reported one group per
/// symbol, and the group order is not stable across *re-indexes* of the
/// same tree: four fresh indexes of `tests/fixtures/polyglot` produced two
/// distinct orderings, while six calls against one store produced one. The
/// instability is in the index, not in this tool, and it predates this
/// lane. Comparing the line multiset keeps the check honest without
/// asserting an order that codegraph does not currently guarantee.
#[tokio::test]
async fn impact_multi_symbol_output_matches_the_captured_wire_output_line_for_line() {
    let (_db, server) = server_over(POLYGLOT, "mcp-multi").await;
    let out = server
        .impact(decode_args::<ImpactParams>(
            "codegraph_impact",
            serde_json::json!({"name": "connect"}),
        ))
        .await;

    const WIRE: &str = "Impact analysis for 'connect': 3 dependent(s) (depth 3):\n\n'connect' matches 2 distinct symbols, shown separately:\n\n-- api::app::connect (api/app.py) --\n  - run (function) in api/consumer.py\n-- worker::connect (worker/main.go) --\n  - Run (function) in worker/main.go\n    - main (function) in worker/main.go\n";

    let mut got: Vec<&str> = out.lines().collect();
    let mut want: Vec<&str> = WIRE.lines().collect();
    got.sort_unstable();
    want.sort_unstable();
    assert_eq!(got, want, "explain off changed the reported lines");
}

#[tokio::test]
async fn impact_explain_on_only_appends() {
    let (_db, server) = server_over(RENAME_AFTER, "mcp-append").await;
    let plain = server
        .impact(decode_args::<ImpactParams>(
            "codegraph_impact",
            serde_json::json!({"name": "helper"}),
        ))
        .await;
    let explained = server
        .impact(decode_args::<ImpactParams>(
            "codegraph_impact",
            serde_json::json!({"name": "helper", "explain": true}),
        ))
        .await;

    assert!(
        explained.starts_with(&plain),
        "explain must append to the report, never rewrite it"
    );
    assert!(
        explained.len() > plain.len(),
        "explain=true produced no chains on a fixture that has a stale reference"
    );
    assert!(explained.contains("## Evidence chains"));
}

/// The MCP tool and the facade produce the same chains.
///
/// `src/main.rs` declares `mod mcp;` without `mod facade;`, so
/// `src/mcp/server.rs` cannot name `crate::facade` and calls
/// `ExplainGraph::load` + `explain::explain_symbol` directly, which is what
/// `facade::explain_chains_for_symbol` does. This test is what stops that
/// from drifting: if the facade grows a step, MCP stops matching and this
/// fails.
#[tokio::test]
async fn mcp_chains_match_the_facade_exactly() {
    let (db, server) = server_over(RENAME_AFTER, "mcp-facade-parity").await;
    let explained = server
        .impact(decode_args::<ImpactParams>(
            "codegraph_impact",
            serde_json::json!({"name": "helper", "explain": true}),
        ))
        .await;

    let over_mcp = chains_from_response(&explained);
    let via_facade = codegraph::facade::explain_chains_for_symbol(&db, "mcp-facade-parity", "helper")
        .await
        .expect("facade chains");

    assert!(!via_facade.is_empty(), "fixture produced no chains at all");
    assert_eq!(
        over_mcp, via_facade,
        "the MCP tool and the facade disagree about this symbol's chains"
    );
}

#[tokio::test]
async fn explained_chains_carry_the_outcome_that_separates_a_rename_from_a_third_party_call() {
    let (_db, server) = server_over(RENAME_AFTER, "mcp-outcome").await;
    let explained = server
        .impact(decode_args::<ImpactParams>(
            "codegraph_impact",
            serde_json::json!({"name": "helper", "explain": true}),
        ))
        .await;

    // The fixture renamed `helper` to `helper_v2` and left one caller, so
    // this is the incomplete-rename outcome, not the "no rule admitted a
    // library symbol" one. An agent told only "UNRESOLVED" cannot tell
    // those apart; the chain has to carry which it is.
    let json = serde_json::to_string(&chains_from_response(&explained)).unwrap();
    assert!(
        json.contains("no_candidates"),
        "the stale chain must report resolution_outcome=no_candidates:\n{explained}"
    );
    assert!(
        !json.contains("no_rule_matched"),
        "this fixture has no unmatched-rule edge, so claiming one would be wrong"
    );
}

#[tokio::test]
async fn explain_limit_truncates_and_says_how_many_it_dropped() {
    let (_db, server) = server_over(RUST_FIXTURE, "mcp-limit").await;
    let all = server
        .impact(decode_args::<ImpactParams>(
            "codegraph_impact",
            serde_json::json!({"name": "helper", "explain": true}),
        ))
        .await;
    let total = chains_from_response(&all).len();
    assert!(total >= 2, "need a symbol with several chains, got {total}");

    let capped = server
        .impact(decode_args::<ImpactParams>(
            "codegraph_impact",
            serde_json::json!({"name": "helper", "explain": true, "explain_limit": 1}),
        ))
        .await;
    assert_eq!(chains_from_response(&capped).len(), 1);
    assert!(
        capped.contains(&format!("1 of {total} shown")),
        "a truncated response must say what it dropped:\n{capped}"
    );
}

// ============================================================================
// D2: codegraph_verify_chain, including tampering across the JSON boundary
// ============================================================================

async fn verdict_for(server: &CodegraphServer, chain_json: serde_json::Value) -> String {
    server
        .verify_chain(decode_args::<VerifyChainParams>(
            "codegraph_verify_chain",
            serde_json::json!({ "chain": chain_json }),
        ))
        .await
}

/// Parse the verdict back out of the tool's response, so assertions are
/// about the structured verdict rather than about prose.
fn verdict_json(text: &str) -> ChainVerdict {
    let start = text.find("```json").expect("verdict JSON block") + "```json".len();
    let rest = &text[start..];
    let end = rest.find("```").expect("unterminated verdict JSON block");
    serde_json::from_str(rest[..end].trim()).expect("verdict parses")
}

/// Overwrite one field of the first step of a given kind, as JSON, and
/// return that step's 1-based index.
///
/// Panics when no such step exists. That matters more than it looks: a
/// tamper test that quietly finds nothing to tamper with passes, and a
/// verifier is exactly the thing a vacuous test cannot be allowed to bless.
/// `Fact` is serde-tagged internally under the key `kind`, so a step
/// serializes as `{"index": n, "fact": {"kind": "<kind>", ...}}` and the
/// payload lives one level deeper than the step. Writing to the wrong level
/// adds an unknown key that serde ignores, which is a no-op tamper. The tag
/// used to be `fact`, which collided with `Step::fact` and made that mistake
/// easy to make and invisible to make; `7e8827e` renamed it after this
/// helper caught exactly that bug.
fn tamper(chain: &mut serde_json::Value, kind: &str, field: &str, to: serde_json::Value) -> usize {
    let steps = chain["steps"].as_array_mut().expect("chain has a steps array");
    for (i, step) in steps.iter_mut().enumerate() {
        if step["fact"]["kind"] == kind {
            let before = step["fact"]
                .get(field)
                .unwrap_or_else(|| {
                    panic!(
                        "step {} of kind {kind} has no field {field}; shape is {}",
                        i + 1,
                        step["fact"]
                    )
                })
                .clone();
            // The tamper has to actually change the JSON. Overwriting a
            // field with the value it already held would leave the chain
            // intact, the verifier would rightly pass it, and the test
            // would read that pass as "tampering was not detected".
            assert_ne!(
                before, to,
                "the tamper is a no-op: step {} field {field} already held {to}",
                i + 1
            );
            step["fact"][field] = to;
            return i + 1;
        }
    }
    panic!("no {kind} step to tamper with in this chain: {chain}");
}

/// The first chain containing a step of the given kind.
fn chain_with_step(chains: &[Chain], kind: &str) -> Chain {
    chains
        .iter()
        .find(|c| {
            serde_json::to_value(c)
                .ok()
                .and_then(|v| v["steps"].as_array().cloned())
                .map(|steps| steps.iter().any(|s| s["fact"]["kind"] == kind))
                .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("no chain in this fixture carries a {kind} step"))
        .clone()
}

#[tokio::test]
async fn a_chain_from_impact_verifies_clean_over_the_json_boundary() {
    let (_db, server) = server_over(RENAME_AFTER, "mcp-verify-clean").await;
    let explained = server
        .impact(decode_args::<ImpactParams>(
            "codegraph_impact",
            serde_json::json!({"name": "helper", "explain": true}),
        ))
        .await;
    let chains = chains_from_response(&explained);
    assert!(!chains.is_empty());

    for chain in &chains {
        // Round-trip through JSON exactly as an agent would: what the tool
        // printed, parsed, re-serialized, sent back.
        let as_json = serde_json::to_value(chain).expect("chain serializes");
        let out = verdict_for(&server, as_json).await;
        let verdict = verdict_json(&out);
        assert!(
            verdict.ok,
            "an untampered chain failed verification:\n{out}"
        );
        assert!(out.contains("verify: PASS"));
        assert!(!verdict.steps.is_empty(), "a chain with no steps proves nothing");
    }
}

/// The tamper D2 asks for: mutate the JSON, not the Rust struct.
#[tokio::test]
async fn verify_chain_rejects_a_swapped_node_id_and_names_the_step() {
    let (_db, server) = server_over(RENAME_AFTER, "mcp-verify-tamper").await;
    let explained = server
        .impact(decode_args::<ImpactParams>(
            "codegraph_impact",
            serde_json::json!({"name": "helper", "explain": true}),
        ))
        .await;
    let chain = chain_with_step(&chains_from_response(&explained), "node_exists");

    // Serialize, mutate one node id as JSON, send it back, verify.
    let mut as_json = serde_json::to_value(&chain).expect("serialize");
    let swapped_at = tamper(
        &mut as_json,
        "node_exists",
        "node_id",
        serde_json::json!("deadbeefdeadbeefdeadbeefdeadbeef"),
    );

    let out = verdict_for(&server, as_json).await;
    let verdict = verdict_json(&out);

    assert!(!verdict.ok, "a swapped node id was accepted:\n{out}");
    let failures = verdict.failures();
    assert!(
        failures.iter().any(|f| f.index == swapped_at && f.fact == "node_exists"),
        "the verdict did not name step {swapped_at} (node_exists) as the failure:\n{out}"
    );
    assert!(out.contains("verify: FAIL"));
    assert!(
        failures.iter().all(|f| f.reason.is_some()),
        "a failing step must say what the graph holds instead"
    );
}

#[tokio::test]
async fn verify_chain_rejects_a_forged_rule_and_a_forged_candidate_pool() {
    let (_db, server) = server_over(RUST_FIXTURE, "mcp-verify-tamper2").await;
    let explained = server
        .impact(decode_args::<ImpactParams>(
            "codegraph_impact",
            serde_json::json!({"name": "helper", "explain": true, "include_ambiguous": true}),
        ))
        .await;
    let chains = chains_from_response(&explained);

    // A rule the cascade never reached.
    let mut forged_rule = serde_json::to_value(chain_with_step(&chains, "rule_application")).unwrap();
    let rule_step = tamper(&mut forged_rule, "rule_application", "rule", serde_json::json!("r99"));
    let out = verdict_for(&server, forged_rule).await;
    let verdict = verdict_json(&out);
    assert!(!verdict.ok, "a forged rule id was accepted:\n{out}");
    assert!(
        verdict
            .failures()
            .iter()
            .any(|f| f.index == rule_step && f.fact == "rule_application"),
        "step {rule_step} (rule_application) should have caught this:\n{out}"
    );

    // An emptied candidate pool. The verifier recomputes the pool rather
    // than trusting the chain, so an emptied one cannot pass.
    let mut forged_pool = serde_json::to_value(chain_with_step(&chains, "candidate_pool")).unwrap();
    let pool_step = tamper(
        &mut forged_pool,
        "candidate_pool",
        "node_ids",
        serde_json::json!([]),
    );
    let out = verdict_for(&server, forged_pool).await;
    let verdict = verdict_json(&out);
    assert!(!verdict.ok, "an emptied candidate pool was accepted:\n{out}");
    assert!(
        verdict
            .failures()
            .iter()
            .any(|f| f.index == pool_step && f.fact == "candidate_pool"),
        "step {pool_step} (candidate_pool) should have caught this:\n{out}"
    );
}

#[tokio::test]
async fn verify_chain_refuses_something_that_is_not_a_chain() {
    let (_db, server) = server_over(POLYGLOT, "mcp-verify-junk").await;

    // An object that is not a chain no longer reaches the handler at all.
    // Since `chain` became the typed `Chain` rather than a loose
    // `serde_json::Value`, the transport's own extractor rejects it, which
    // is strictly better than refusing it inside the tool: the caller gets
    // a protocol-level error naming the field, and there is no code path in
    // which an empty chain could be constructed and then "verified".
    let rejected = serde_json::from_value::<VerifyChainParams>(
        serde_json::json!({"chain": {"not": "a chain"}}),
    );
    assert!(
        rejected.is_err(),
        "a non-chain object must be refused at the MCP boundary, not defaulted into an empty pass"
    );

    // A string that is not a chain is admitted by the schema (a string is a
    // valid ChainArg) and refused one layer in, by the tool, with a message
    // that says what to send instead.
    let out = verdict_for(&server, serde_json::json!("not a chain at all")).await;
    assert!(
        out.starts_with("Error:") && out.contains("not a codegraph evidence chain"),
        "a string that is not a chain must be refused with guidance:\n{out}"
    );

    // A chain arriving as a JSON string is still a chain.
    let (_db2, server2) = server_over(RENAME_AFTER, "mcp-verify-str").await;
    let explained = server2
        .impact(decode_args::<ImpactParams>(
            "codegraph_impact",
            serde_json::json!({"name": "helper", "explain": true}),
        ))
        .await;
    let chain = chains_from_response(&explained).remove(0);
    let stringified = serde_json::Value::String(serde_json::to_string(&chain).unwrap());
    let out = verdict_for(&server2, stringified).await;
    assert!(verdict_json(&out).ok, "a stringified chain was rejected:\n{out}");
}

/// Independent of the tool: the same chain re-checked directly must agree
/// with what the tool said, so the tool is not softening a verdict.
#[tokio::test]
async fn the_tool_reports_the_same_verdict_the_library_produces() {
    let (db, server) = server_over(RENAME_AFTER, "mcp-verdict-parity").await;
    let explained = server
        .impact(decode_args::<ImpactParams>(
            "codegraph_impact",
            serde_json::json!({"name": "helper", "explain": true}),
        ))
        .await;
    let chain = chains_from_response(&explained).remove(0);

    let graph = ExplainGraph::load(&db, "mcp-verdict-parity").await.unwrap();
    let direct = explain::verify_chain(&chain, &graph);

    let out = verdict_for(&server, serde_json::to_value(&chain).unwrap()).await;
    assert_eq!(verdict_json(&out), direct);
}

// ============================================================================
// D3: codegraph_architecture.file_filter
// ============================================================================

/// Fails on the tree before this lane.
///
/// There, `architecture` ran `let _ = p.file_filter;` and reported the whole
/// project, so `filtered` equalled `unfiltered` and both listed every
/// language. Captured from the pre-change binary over MCP stdio: the
/// responses for `{}`, `{"file_filter":"api/"}`,
/// `{"file_filter":"client/**"}` and `{"file_filter":"**/does_not_exist/**"}`
/// were all the same string.
#[tokio::test]
async fn architecture_file_filter_actually_narrows_the_report() {
    let (_db, server) = server_over(POLYGLOT, "mcp-filter").await;

    let unfiltered = server
        .architecture(decode_args::<ArchitectureParams>(
            "codegraph_architecture",
            serde_json::json!({}),
        ))
        .await;
    let filtered = server
        .architecture(decode_args::<ArchitectureParams>(
            "codegraph_architecture",
            serde_json::json!({"file_filter": "api/"}),
        ))
        .await;

    assert_ne!(
        unfiltered, filtered,
        "file_filter changed nothing, which is the defect this test exists for"
    );

    // polyglot is api/*.py, client/src/*.ts and worker/main.go. Scoped to
    // api/, only python may appear.
    assert!(filtered.contains("python"), "api/ is python:\n{filtered}");
    assert!(
        !filtered.contains("typescript"),
        "typescript is outside api/ and must not be counted:\n{filtered}"
    );
    assert!(
        !filtered.contains("\n- go: "),
        "go is outside api/ and must not be counted:\n{filtered}"
    );
    assert!(
        unfiltered.contains("typescript"),
        "sanity: the unfiltered report should list typescript"
    );

    // Hubs are narrowed too, not just the counts.
    let hub_section = filtered.split("## Hub Nodes").nth(1).expect("hub section");
    assert!(
        hub_section.contains("api/"),
        "hubs should still be reported inside the scope:\n{filtered}"
    );
    assert!(
        !hub_section.contains("worker/main.go") && !hub_section.contains("client/src"),
        "hubs outside the scope leaked in:\n{filtered}"
    );

    // The report has to state what it did, or a reader cannot tell a narrow
    // scope from an empty project.
    assert!(filtered.contains("file_filter"));
    assert!(filtered.contains("substring"));
}

#[tokio::test]
async fn architecture_file_filter_accepts_a_glob() {
    let (_db, server) = server_over(POLYGLOT, "mcp-filter-glob").await;

    let ts = server
        .architecture(decode_args::<ArchitectureParams>(
            "codegraph_architecture",
            serde_json::json!({"file_filter": "client/**"}),
        ))
        .await;
    assert!(ts.contains("glob"), "a * pattern must be matched as a glob:\n{ts}");
    assert!(ts.contains("typescript"));
    assert!(!ts.contains("python"), "python is outside client/:\n{ts}");

    let deep = server
        .architecture(decode_args::<ArchitectureParams>(
            "codegraph_architecture",
            serde_json::json!({"file_filter": "**/*.go"}),
        ))
        .await;
    assert!(deep.contains("\n- go: "), "**/*.go should find worker/main.go:\n{deep}");
    assert!(!deep.contains("typescript"));
}

#[tokio::test]
async fn architecture_file_filter_that_matches_nothing_says_so() {
    let (_db, server) = server_over(POLYGLOT, "mcp-filter-empty").await;
    let out = server
        .architecture(decode_args::<ArchitectureParams>(
            "codegraph_architecture",
            serde_json::json!({"file_filter": "**/does_not_exist/**"}),
        ))
        .await;

    assert!(
        out.contains("0 of "),
        "an empty scope must report that it matched nothing:\n{out}"
    );
    assert!(
        out.contains("Nothing matched"),
        "and say so in words, rather than printing an empty report:\n{out}"
    );
    // An empty scope is not an empty project, and must not read like one.
    assert!(!out.contains("\n- function: "), "counts leaked into an empty scope:\n{out}");
}

#[tokio::test]
async fn architecture_without_a_filter_is_byte_identical_to_the_captured_wire_output() {
    let (_db, server) = server_over(POLYGLOT, "mcp-arch-plain").await;
    let out = server
        .architecture(decode_args::<ArchitectureParams>(
            "codegraph_architecture",
            serde_json::json!({}),
        ))
        .await;
    assert!(
        out.starts_with(WIRE_POLYGLOT_ARCH_NODE_TYPES),
        "the unfiltered summary drifted from the captured wire bytes:\n{out}"
    );

    // An empty string is not a filter, and must take the unfiltered path.
    let empty = server
        .architecture(decode_args::<ArchitectureParams>(
            "codegraph_architecture",
            serde_json::json!({"file_filter": ""}),
        ))
        .await;
    assert_eq!(empty, out);
}

// ============================================================================
// D4: codegraph_clones
// ============================================================================

#[tokio::test]
async fn clones_tool_reports_what_the_library_groups() {
    let (db, server) = server_over(RUST_FIXTURE, "mcp-clones").await;

    let groups = find_clone_groups(&db, "mcp-clones", 1).await.expect("library groups");
    // The comparison below is only worth anything against a non-empty
    // result: "the tool reported nothing and so did the library" would pass
    // on a tool that always reports nothing. If this fixture ever stops
    // producing groups, this test needs a different fixture, not a softer
    // assertion.
    assert!(
        !groups.is_empty(),
        "the rust fixture produced no clone groups at min_edges=1, so this test would be vacuous"
    );

    let out = server
        .clones(decode_args::<ClonesParams>(
            "codegraph_clones",
            serde_json::json!({"min_edges": 1}),
        ))
        .await;

    assert!(
        out.contains(&format!("{} group(s)", groups.len())),
        "tool and library disagree on the group count ({}):\n{out}",
        groups.len()
    );
    for g in &groups {
        assert!(
            out.contains(&g.fingerprint),
            "group {} is missing from the tool output:\n{out}",
            g.fingerprint
        );
        for m in &g.members {
            assert!(
                out.contains(&m.file_path),
                "member {} is missing from the tool output:\n{out}",
                m.qualified_name
            );
        }
    }
}

#[tokio::test]
async fn clones_min_edges_is_the_documented_threshold() {
    let (db, server) = server_over(RUST_FIXTURE, "mcp-clones-thresh").await;

    let low = find_clone_groups(&db, "mcp-clones-thresh", 1).await.unwrap().len();
    let high = find_clone_groups(&db, "mcp-clones-thresh", 99).await.unwrap().len();
    assert_eq!(high, 0, "no neighborhood has 99 edges in this fixture");
    assert!(low >= high, "raising min_edges must not add groups");

    let out = server
        .clones(decode_args::<ClonesParams>(
            "codegraph_clones",
            serde_json::json!({"min_edges": 99}),
        ))
        .await;
    assert!(out.starts_with("No structural clones"), "{out}");
    assert!(
        out.contains("min_edges=99"),
        "an empty result must name the threshold that produced it:\n{out}"
    );
    // The project has fingerprints; "nothing matched" must not be reported
    // as "nothing was compared".
    assert!(
        !out.contains("no fingerprint rows at all"),
        "this project is fingerprinted, so that message would be false:\n{out}"
    );
    assert!(out.contains("fingerprinted symbol(s)"), "{out}");
}

#[tokio::test]
async fn clones_distinguishes_no_clones_from_nothing_compared() {
    // An indexed-but-unfingerprinted project cannot be produced through
    // `index_project` (the fingerprint pass is step 5b and always runs), so
    // this checks the other side: a project id with nothing under it at all.
    let db = fresh_db().await.unwrap();
    let server = CodegraphServer::new(Arc::clone(&db), "mcp-clones-empty".to_string());
    let out = server
        .clones(decode_args::<ClonesParams>(
            "codegraph_clones",
            serde_json::json!({}),
        ))
        .await;
    assert!(
        out.contains("no fingerprint rows at all"),
        "an unindexed project must not read as 'no clones found':\n{out}"
    );
}

// ============================================================================
// D5: descriptions written for an agent reader
// ============================================================================

#[tokio::test]
async fn every_tool_is_registered_and_described_for_an_agent() {
    let router = CodegraphServer::tool_router();
    let tools = router.list_all();
    let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();

    for expected in [
        "codegraph_search",
        "codegraph_impact",
        "codegraph_verify_chain",
        "codegraph_architecture",
        "codegraph_quality",
        "codegraph_clones",
    ] {
        assert!(names.contains(&expected.to_string()), "missing tool {expected}; have {names:?}");
    }

    for t in &tools {
        let d = t
            .description
            .as_deref()
            .unwrap_or_else(|| panic!("{} has no description", t.name));

        assert!(
            d.starts_with("Answers:"),
            "{}'s description must open with the question it answers",
            t.name
        );
        for term in ["RESOLVED", "AMBIGUOUS", "UNRESOLVED", "include_ambiguous"] {
            assert!(
                d.contains(term),
                "{}'s description never mentions {term}, so an agent cannot tell whether it \
                 applies here",
                t.name
            );
        }
        // House style: no em dash, and no en dash standing in for one.
        assert!(!d.contains('\u{2014}'), "{} has an em dash in its description", t.name);
        assert!(
            !d.contains(" \u{2013} "),
            "{} has an en dash used as punctuation",
            t.name
        );
        assert!(
            d.len() > 400,
            "{}'s description is {} chars, too thin to tell an agent when to reach for it",
            t.name,
            d.len()
        );
    }
}

#[tokio::test]
async fn the_impact_description_warns_about_the_unresolved_distinction() {
    let router = CodegraphServer::tool_router();
    let d = router
        .get("codegraph_impact")
        .and_then(|t| t.description.clone())
        .expect("codegraph_impact description");

    // L7's finding with commercial weight: reporting both unresolved
    // outcomes as an incomplete rename would mislead an audit client.
    assert!(d.contains("no_candidates"));
    assert!(d.contains("no_rule_matched"));
    assert!(d.contains("NOT automatically an incomplete rename"));
    // And the chain contract an agent needs before trusting one.
    assert!(d.contains("re-checkable") || d.contains("re-derives"));
    assert!(d.contains("codegraph_verify_chain"));
}

// ============================================================================
// Part two: the planes tools
//
// The fixture is lane L2's own `ops-rust.yaml` against `tests/fixtures/rust`,
// reused deliberately rather than written fresh: its verdicts are facts about
// that fixture recorded in its `expected.yaml`, so these tests and the
// resolver's tests are pinned to one ground truth instead of two. What is
// under test here is the MCP surface, not the roadmap engine, so every
// assertion is about a field an agent reads out of the tool response.
// ============================================================================

const PLAN_PROJECT: &str = "mcp-plan";
const PLAN_FIXTURE: &str = "tests/fixtures/rust";
const PLAN_FILE: &str = "tests/fixtures/planes/ops-rust.yaml";

/// A server over the rust fixture with L2's roadmap synced in, through the
/// MCP tool rather than through `ops::sync`, so the sync path is exercised
/// by every test that depends on it.
async fn planned_server() -> (Arc<Surreal<Any>>, CodegraphServer) {
    let (db, server) = server_over(PLAN_FIXTURE, PLAN_PROJECT).await;
    let out = plan_sync(&server, &common::repo_path(PLAN_FILE).to_string_lossy()).await;
    assert!(
        !out.starts_with("Error:"),
        "the fixture roadmap must sync cleanly:\n{out}"
    );
    (db, server)
}

async fn plan_sync(server: &CodegraphServer, path: &str) -> String {
    server
        .plan_sync(decode_args::<PlanSyncParams>(
            "codegraph_plan_sync",
            serde_json::json!({ "path": path }),
        ))
        .await
}

/// The JSON report an agent parses out of a planes tool response.
fn report_of(text: &str) -> serde_json::Value {
    let start = text
        .find("```json")
        .unwrap_or_else(|| panic!("no JSON block in the response:\n{text}"))
        + "```json".len();
    let rest = &text[start..];
    let end = rest
        .find("```")
        .unwrap_or_else(|| panic!("unterminated JSON block in:\n{text}"));
    serde_json::from_str(rest[..end].trim())
        .unwrap_or_else(|e| panic!("the JSON block does not parse: {e}\n{}", &rest[..end]))
}

/// Item ids in a `touching` report.
fn hit_items(report: &serde_json::Value) -> Vec<String> {
    report["hits"]
        .as_array()
        .expect("hits array")
        .iter()
        .map(|h| h["item"]["item_id"].as_str().expect("item_id").to_string())
        .collect()
}

#[tokio::test]
async fn plan_touching_finds_planned_work_by_file_and_by_symbol() {
    let (_db, server) = planned_server().await;

    // By file. OPS-1, OPS-2 and OPS-5 all touch src/main.rs.
    let out = server
        .plan_touching(decode_args::<PlanTouchingParams>(
            "codegraph_plan_touching",
            serde_json::json!({"target": "src/main.rs"}),
        ))
        .await;
    let report = report_of(&out);
    let items = hit_items(&report);
    for want in ["OPS-1", "OPS-2", "OPS-5"] {
        assert!(items.contains(&want.to_string()), "{want} missing from {items:?}\n{out}");
    }

    // By symbol. `helper` is defined in both alpha.rs and beta.rs, so
    // OPS-1's `- symbol: helper` is AMBIGUOUS, and the hit has to say so
    // rather than present itself as coverage.
    let out = server
        .plan_touching(decode_args::<PlanTouchingParams>(
            "codegraph_plan_touching",
            serde_json::json!({"target": "helper"}),
        ))
        .await;
    let report = report_of(&out);
    let items = hit_items(&report);
    assert!(items.contains(&"OPS-1".to_string()), "{items:?}\n{out}");
    let matched: Vec<&str> = report["hits"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|h| h["item"]["item_id"] == "OPS-1")
        .map(|h| h["matched_by"].as_str().unwrap())
        .collect();
    assert!(
        matched.iter().any(|m| *m == "candidate" || *m == "literal"),
        "an ambiguous touch must not be reported as bound coverage: {matched:?}\n{out}"
    );
}

#[tokio::test]
async fn plan_touching_separates_nothing_planned_from_nothing_named() {
    let (_db, server) = planned_server().await;

    // A real symbol with no planned work: target_ids is non-empty, so the
    // empty hit list is meaningful.
    let out = server
        .plan_touching(decode_args::<PlanTouchingParams>(
            "codegraph_plan_touching",
            serde_json::json!({"target": "no_such_name_in_this_fixture_at_all"}),
        ))
        .await;
    let report = report_of(&out);
    assert!(report["hits"].as_array().unwrap().is_empty());
    assert!(
        report["target_ids"].as_array().unwrap().is_empty(),
        "a name nothing answers to must have no target_ids"
    );
    assert!(
        out.contains("nothing in the index answers to that name"),
        "the summary must not let an agent read this as 'unclaimed':\n{out}"
    );
}

#[tokio::test]
async fn plan_list_filters_and_refuses_an_unknown_filter_value() {
    let (_db, server) = planned_server().await;

    let all = report_of(
        &server
            .plan_list(decode_args::<PlanListParams>(
                "codegraph_plan_list",
                serde_json::json!({}),
            ))
            .await,
    );
    let planes = all["planes"].as_array().unwrap().len();
    assert_eq!(planes, 2, "the fixture has two planes");

    let active = report_of(
        &server
            .plan_list(decode_args::<PlanListParams>(
                "codegraph_plan_list",
                serde_json::json!({"status": "active"}),
            ))
            .await,
    );
    let active_items: Vec<&str> = active["planes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|p| p["items"].as_array().unwrap())
        .map(|i| i["item_id"].as_str().unwrap())
        .collect();
    assert!(active_items.contains(&"OPS-1"), "{active_items:?}");
    assert!(
        !active_items.contains(&"OPS-5"),
        "OPS-5 is planned, not active: {active_items:?}"
    );

    // An unknown value is refused with the accepted list, never silently
    // dropped: a dropped filter turns "no items are active" into "here is
    // everything", which is the wrong answer delivered confidently.
    let bad = server
        .plan_list(decode_args::<PlanListParams>(
            "codegraph_plan_list",
            serde_json::json!({"status": "in-progress"}),
        ))
        .await;
    assert!(bad.starts_with("Error:"), "{bad}");
    assert!(bad.contains("active"), "the error must list what is accepted:\n{bad}");
}

#[tokio::test]
async fn plan_show_returns_the_touch_set_with_confidences() {
    let (_db, server) = planned_server().await;
    let out = server
        .plan_show(decode_args::<PlanShowParams>(
            "codegraph_plan_show",
            serde_json::json!({"item_id": "OPS-1"}),
        ))
        .await;
    let report = report_of(&out);

    assert_eq!(report["item"]["item_id"], "OPS-1");
    assert_eq!(report["plane"]["plane_id"], "core");
    // OPS-1 was built to carry one of every confidence.
    assert!(report["item"]["resolved"].as_u64().unwrap() >= 1, "{out}");
    assert_eq!(report["item"]["ambiguous"].as_u64().unwrap(), 1, "{out}");
    assert!(report["item"]["unresolved"].as_u64().unwrap() >= 2, "{out}");
    // Every unbound touch carries a reason, which is the field that tells a
    // permanently unbindable document apart from a renamed function.
    let unbound = report["unbound"].as_array().expect("unbound array");
    assert!(!unbound.is_empty(), "{out}");
    assert!(
        unbound.iter().all(|u| u.get("reason").is_some()),
        "every unbound touch must say why it did not bind:\n{out}"
    );
    // OPS-2 declares depends_on OPS-1, so OPS-1 blocks it.
    assert!(
        report["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b == "OPS-2"),
        "OPS-1 should report that it blocks OPS-2:\n{out}"
    );
}

#[tokio::test]
async fn plan_collisions_finds_the_active_pair_and_ignores_the_planned_one() {
    let (_db, server) = planned_server().await;
    let out = server.plan_collisions().await;
    let report = report_of(&out);

    let pairs: Vec<(String, String)> = report["collisions"]
        .as_array()
        .expect("collisions array")
        .iter()
        .map(|c| {
            (
                c["a"]["item_id"].as_str().unwrap().to_string(),
                c["b"]["item_id"].as_str().unwrap().to_string(),
            )
        })
        .collect();

    // OPS-1 and OPS-2 are both active and both touch src/main.rs.
    assert!(
        pairs
            .iter()
            .any(|(a, b)| (a == "OPS-1" && b == "OPS-2") || (a == "OPS-2" && b == "OPS-1")),
        "the active pair sharing src/main.rs is missing: {pairs:?}\n{out}"
    );
    // OPS-5 touches src/main.rs too, but it is planned, not active, so it
    // must not be reported as a collision with anything.
    assert!(
        !pairs.iter().any(|(a, b)| a == "OPS-5" || b == "OPS-5"),
        "OPS-5 is planned and must not collide: {pairs:?}\n{out}"
    );
    // OPS-3 is an active decoy that shares nothing.
    assert!(
        !pairs.iter().any(|(a, b)| a == "OPS-3" || b == "OPS-3"),
        "OPS-3 shares no code: {pairs:?}\n{out}"
    );
}

#[tokio::test]
async fn plan_stale_names_the_item_and_keeps_actionable_separate() {
    let (_db, server) = planned_server().await;
    let out = server.plan_stale().await;
    let report = report_of(&out);

    let stale: Vec<&str> = report["items"]
        .as_array()
        .expect("items array")
        .iter()
        .map(|i| i["item"]["item_id"].as_str().unwrap())
        .collect();
    assert!(
        stale.contains(&"OPS-1"),
        "OPS-1 points at a symbol and a file that do not exist: {stale:?}\n{out}"
    );

    // The number a reader acts on is reported separately from the raw
    // total, and the reason tally says which reasons can be acted on.
    assert!(report["actionable"].is_number(), "{out}");
    let by_reason = report["by_reason"].as_array().expect("by_reason array");
    assert!(!by_reason.is_empty(), "{out}");
    assert!(
        by_reason.iter().all(|r| r["actionable"].is_boolean()),
        "every reason must say whether it can be acted on:\n{out}"
    );
}

#[tokio::test]
async fn plan_blast_returns_the_documented_callers() {
    let (_db, server) = planned_server().await;
    let out = server
        .plan_blast(decode_args::<PlanBlastParams>(
            "codegraph_plan_blast",
            serde_json::json!({"item_id": "OPS-4", "depth": 1}),
        ))
        .await;
    let report = report_of(&out);

    assert_eq!(report["item_id"], "OPS-4");
    assert_eq!(report["depth"], 1);
    // OPS-4 touches `connect`, which the rust fixture's own expected.yaml
    // records as having exactly two RESOLVED callers: `run` (case b) and
    // `call_connect_via_module_import` (case bonus-r2).
    let reached: BTreeSet<&str> = report["reached"]
        .as_array()
        .expect("reached array")
        .iter()
        .map(|h| h["name"].as_str().unwrap())
        .collect();
    assert!(reached.contains("run"), "{reached:?}\n{out}");
    assert!(
        reached.contains("call_connect_via_module_import"),
        "{reached:?}\n{out}"
    );
}

#[tokio::test]
async fn plan_sync_refuses_an_invalid_file_and_writes_nothing() {
    let (_db, server) = planned_server().await;

    // What the store holds before the bad sync.
    let before = report_of(
        &server
            .plan_list(decode_args::<PlanListParams>(
                "codegraph_plan_list",
                serde_json::json!({}),
            ))
            .await,
    );

    let bad = plan_sync(
        &server,
        &common::repo_path("tests/fixtures/planes/dependency-cycle.yaml").to_string_lossy(),
    )
    .await;
    assert!(bad.starts_with("Error:"), "an invalid roadmap must be refused:\n{bad}");
    assert!(
        bad.contains("Nothing was written"),
        "the refusal must say the stored plan is untouched, or a caller will assume a partial \
         roadmap landed:\n{bad}"
    );

    // And the store is byte-for-byte what it was.
    let after = report_of(
        &server
            .plan_list(decode_args::<PlanListParams>(
                "codegraph_plan_list",
                serde_json::json!({}),
            ))
            .await,
    );
    assert_eq!(before, after, "a refused sync must not have changed the stored plan");
}

#[tokio::test]
async fn plan_sync_reports_the_confidence_split_and_the_index_it_resolved_against() {
    let (_db, server) = server_over(PLAN_FIXTURE, "mcp-plan-sync").await;
    let out = plan_sync(&server, &common::repo_path(PLAN_FILE).to_string_lossy()).await;
    let report = report_of(&out);

    assert_eq!(report["planes"], 2);
    assert_eq!(report["items"], 6);
    // The three tallies must partition the touches, or a reader cannot
    // trust any one of them.
    let (r, a, u) = (
        report["resolved"].as_u64().unwrap(),
        report["ambiguous"].as_u64().unwrap(),
        report["unresolved"].as_u64().unwrap(),
    );
    assert_eq!(r + a + u, report["touches"].as_u64().unwrap(), "{out}");
    // indexed_files above zero is what tells a real roadmap problem apart
    // from a project that was simply never indexed.
    assert!(
        report["indexed_files"].as_u64().unwrap() > 0,
        "the fixture was indexed, so this must not be zero:\n{out}"
    );
}
