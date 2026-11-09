"use strict";
/*
 * Unit tests for the UDM Handoff extension logic (extension/handoff.js).
 * Run:  node extension/tests/handoff.test.js
 * No dependencies beyond Node itself.
 */
const assert = require("node:assert");
const path = require("node:path");
const H = require(path.join(__dirname, "..", "handoff.js"));

let passed = 0;
async function test(name, fn) {
  await fn();
  passed += 1;
  console.log(`ok - ${name}`);
}

function item(overrides) {
  return Object.assign(
    {
      id: 42,
      url: "https://files.example.com/pub/report.pdf",
      finalUrl: "",
      filename: "C:\\Users\\x\\Downloads\\report.pdf",
      fileSize: 12345,
      state: "in_progress",
    },
    overrides,
  );
}

(async () => {
  // ---- eligibility ----------------------------------------------------
  await test("eligible by default for http(s)", () => {
    assert.strictEqual(H.isEligible(item(), { enabled: true, exclusions: [] }), true);
    assert.strictEqual(H.isEligible(item({ url: "http://a.b/c.bin" }), { enabled: true, exclusions: [] }), true);
  });

  await test("disabled or non-http(s) or finished downloads are not eligible", () => {
    assert.strictEqual(H.isEligible(item(), { enabled: false, exclusions: [] }), false);
    assert.strictEqual(H.isEligible(item({ url: "ftp://a.b/c" }), { enabled: true, exclusions: [] }), false);
    assert.strictEqual(H.isEligible(item({ url: "blob:https://a.b/x" }), { enabled: true, exclusions: [] }), false);
    assert.strictEqual(H.isEligible(item({ state: "complete" }), { enabled: true, exclusions: [] }), false);
  });

  await test("exclusions: host, host suffix, extension, substring", () => {
    const s = (exclusions) => ({ enabled: true, exclusions });
    assert.strictEqual(H.isEligible(item(), s(["files.example.com"])), false);
    assert.strictEqual(H.isEligible(item(), s(["*.example.com"])), false);
    assert.strictEqual(H.isEligible(item(), s([".pdf"])), false);
    assert.strictEqual(H.isEligible(item(), s(["report"])), false);
    assert.strictEqual(H.isEligible(item(), s(["other.example.com", ".iso"])), true);
    assert.strictEqual(H.isEligible(item({ url: "https://sub.example.com/x" }), s(["*.example.com"])), false);
    assert.strictEqual(H.isEligible(item({ url: "https://example.com/x" }), s(["*.example.com"])), false);
  });

  // ---- intent ---------------------------------------------------------
  await test("buildIntent carries the frozen fields", () => {
    const i = H.buildIntent(item());
    assert.strictEqual(i.url, "https://files.example.com/pub/report.pdf");
    assert.strictEqual(i.method, "GET");
    assert.strictEqual(i.filename_hint, "report.pdf");
    assert.strictEqual(i.size_hint, 12345);
    assert.strictEqual(i.browser_download_id, "42");
    assert.strictEqual(i.dest_dir, null);
    const j = H.buildIntent(item({ finalUrl: "https://cdn.example.com/final.pdf", fileSize: -1 }));
    assert.strictEqual(j.url, "https://cdn.example.com/final.pdf");
    assert.strictEqual(j.size_hint, null);
  });

  // ---- wire client ----------------------------------------------------
  function fakePort() {
    const listeners = { msg: [], disc: [] };
    return {
      sent: [],
      postMessage(m) {
        this.sent.push(m);
      },
      onMessage: { addListener: (f) => listeners.msg.push(f) },
      onDisconnect: { addListener: (f) => listeners.disc.push(f) },
      disconnect() {},
      _reply(m) {
        listeners.msg.forEach((f) => f(m));
      },
      _disconnect() {
        listeners.disc.forEach((f) => f());
      },
    };
  }

  await test("client serializes requests and matches replies in order", async () => {
    const port = fakePort();
    const client = H.createHandoffClient(() => port);
    const p1 = client.prepare("tx-1", { url: "https://a.b/f" });
    const p2 = client.query("tx-1");
    // Only the first request is on the wire so far.
    assert.strictEqual(port.sent.length, 1);
    assert.strictEqual(port.sent[0].op, "prepare");
    assert.strictEqual(port.sent[0].v, 1);
    port._reply({ v: 1, op: "prepare", tx: "tx-1", status: "prepared" });
    const r1 = await p1;
    assert.strictEqual(r1.status, "prepared");
    assert.strictEqual(port.sent.length, 2);
    assert.strictEqual(port.sent[1].op, "query");
    port._reply({ v: 1, op: "query", tx: "tx-1", status: "not-found" });
    assert.strictEqual((await p2).status, "not-found");
  });

  await test("client resolves unavailable when the host cannot start or drops", async () => {
    const none = H.createHandoffClient(() => null);
    assert.strictEqual((await none.query("tx")).status, "unavailable");
    const port = fakePort();
    const client = H.createHandoffClient(() => port);
    const p = client.commit("tx-9");
    port._disconnect();
    assert.strictEqual((await p).status, "unavailable");
  });

  // ---- handoff flow ---------------------------------------------------
  function fakeFx(behavior) {
    const calls = [];
    return {
      calls,
      pause: async (id) => (calls.push(["pause", id]), true),
      resume: async (id) => (calls.push(["resume", id]), true),
      cancel: async (id) => (calls.push(["cancel", id]), behavior.cancel),
      erase: async (id) => calls.push(["erase", id]),
      record: async (e) => calls.push(["record", e.status]),
    };
  }
  function fakeClient(replies) {
    const calls = [];
    const next = (op) => async (tx) => (calls.push(op), replies[op]);
    return { calls, prepare: next("prepare"), commit: next("commit"), abort: next("abort"), query: next("query") };
  }

  await test("flow: happy path hands off exactly once", async () => {
    const fx = fakeFx({ cancel: true });
    const client = fakeClient({
      prepare: { status: "prepared" },
      commit: { status: "committed", task_ref: "1001" },
    });
    const out = await H.createHandoffFlow(client, fx).handoff(item());
    assert.strictEqual(out, "handed-off");
    assert.deepStrictEqual(client.calls, ["prepare", "commit"]);
    assert.ok(fx.calls.some((c) => c[0] === "cancel"));
    assert.ok(fx.calls.some((c) => c[0] === "record" && c[1] === "handed-off"));
  });

  await test("flow: coordinator unavailable keeps the browser download", async () => {
    const fx = fakeFx({ cancel: true });
    const client = fakeClient({ prepare: { status: "unavailable" } });
    const out = await H.createHandoffFlow(client, fx).handoff(item());
    assert.strictEqual(out, "kept-by-browser");
    assert.deepStrictEqual(client.calls, ["prepare"]);
    assert.ok(fx.calls.some((c) => c[0] === "resume"));
    assert.ok(!fx.calls.some((c) => c[0] === "cancel"));
  });

  await test("flow: unconfirmed cancel aborts and never commits", async () => {
    const fx = fakeFx({ cancel: false });
    const client = fakeClient({ prepare: { status: "prepared" }, abort: { status: "aborted" } });
    const out = await H.createHandoffFlow(client, fx).handoff(item());
    assert.strictEqual(out, "kept-by-browser");
    assert.deepStrictEqual(client.calls, ["prepare", "abort"]);
  });

  await test("flow: expired commit surfaces recovery-required", async () => {
    const fx = fakeFx({ cancel: true });
    const client = fakeClient({
      prepare: { status: "prepared" },
      commit: { status: "recovery-required" },
    });
    const out = await H.createHandoffFlow(client, fx).handoff(item());
    assert.strictEqual(out, "recovery-required");
    assert.deepStrictEqual(client.calls, ["prepare", "commit"]);
  });

  console.log(`\n${passed} tests passed`);
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
