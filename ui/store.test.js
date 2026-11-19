// Pure-logic tests for the client state machine, run with `node --test ui/`.
// The DOM-dependent modules are not imported here; store.js touches the
// document only inside setView, which these tests never call.

import test from "node:test";
import assert from "node:assert/strict";
import { beforeEach } from "node:test";

import { state, classify, nextAction, receiptState } from "./js/store.js";

function task(partial = {}) {
  return {
    id: "t_test01",
    title: "test",
    status: "done",
    branch: "orch/test-t_test01",
    sessionId: "s1",
    verify: null,
    ...partial,
  };
}

function beforeEachFn() {
  state.settings = { checks: ["cargo test"] };
  state.behind = {};
}

beforeEach(beforeEachFn);

/* ---------------------------------------------------------- receiptState */

test("receiptState prefers the server verdict", () => {
  const t = task({ receiptState: "stale", verify: { status: "passed" } });
  assert.equal(receiptState(t, 0, true), "stale");
});

test("receiptState falls back to local derivation", () => {
  assert.equal(receiptState(task(), 0, true), "unverified");
  assert.equal(receiptState(task(), 0, false), "ungated");
  assert.equal(receiptState(task({ verify: { status: "failed" } }), 0, true), "failed");
  assert.equal(
    receiptState(task({ verify: { status: "needs_review" } }), 2, true),
    "needs_review",
  );
  assert.equal(receiptState(task({ verify: { status: "passed" } }), 2, true), "stale");
  assert.equal(receiptState(task({ verify: { status: "passed" } }), 0, false), "passed");
});

/* -------------------------------------------------------------- classify */

test("classify ranks evidence worst-first", () => {
  assert.equal(classify(task({ status: "error" })).kind, "error");
  assert.equal(classify(task({ verify: { status: "failed" } })).kind, "failed");
  assert.equal(classify(task({ verify: { status: "needs_review" } })).kind, "policy");
  state.behind.t_test01 = 3;
  assert.equal(classify(task({ verify: { status: "passed" } })).kind, "stale");
  delete state.behind.t_test01;
  assert.equal(classify(task()).kind, "review"); // done + unverified
});

test("classify leaves calm tasks quiet", () => {
  // Passed receipts need nobody.
  assert.equal(classify(task({ verify: { status: "passed" } })), null);
  // A task that has not finished needs no gate yet.
  assert.equal(classify(task({ status: "idle" })), null);
});

test("classify distinguishes unverified from ungated only when done", () => {
  state.settings = { checks: [] };
  assert.equal(classify(task({ status: "idle" })), null);
  assert.equal(classify(task({ status: "running" })), null);
  const info = classify(task());
  assert.equal(info.kind, "review");
  assert.equal(info.rank, 5);
  assert.equal(info.reason, "done — no gate configured");
});

/* ------------------------------------------------------------ nextAction */

test("nextAction maps each state to its decisive move", () => {
  assert.equal(nextAction(task({ verify: { status: "failed" } }), 0, true).id, "instruct");
  assert.equal(nextAction(task({ verify: { status: "needs_review" } }), 0, true).id, "merge");
  assert.equal(nextAction(task({ verify: { status: "passed" } }), 3, true).id, "rebase");
  assert.equal(nextAction(task({ verify: { status: "passed" } }), 0, true).id, "merge");
  assert.equal(nextAction(task(), 0, true).id, "verify");
  assert.equal(nextAction(task(), 0, false).id, "merge");
});

test("nextAction stays silent while the agent works or after landing", () => {
  assert.equal(nextAction(task({ status: "running" }), 0, true), null);
  assert.equal(nextAction(task({ status: "waiting" }), 0, true), null);
  assert.equal(nextAction(task({ status: "merged", verify: { status: "passed" } })), null);
});

test("nextAction points an unstarted task at the composer", () => {
  const t = task({ status: "idle", branch: "", sessionId: null });
  const action = nextAction(t, 0, true);
  assert.equal(action.id, "instruct");
  assert.equal(action.label, "describe the task");
});

test("error outranks every receipt", () => {
  const t = task({ status: "error", verify: { status: "passed" } });
  assert.equal(nextAction(t, 0, true).id, "instruct");
});
