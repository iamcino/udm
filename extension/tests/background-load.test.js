"use strict";
/*
 * Regression test: the extension background must load in a real browser.
 * Firefox loads handoff.js then background.js as background scripts in ONE
 * shared lexical scope (Chrome shares the worker global lexical scope via
 * importScripts), so a top-level `const api` in both files is a parse-time
 * SyntaxError and the background never runs. The require()-based unit tests
 * load handoff.js standalone and cannot see this; model the shared scope
 * with node:vm instead.
 *
 * The same harness pins the browser detection: current Chrome exposes a
 * `browser` global (without runtime.getBrowserInfo), so the mere presence
 * of `browser` must NOT select the Firefox native host - that misdetection
 * made every Chrome handoff fail with "host not found".
 * Run:  node extension/tests/background-load.test.js
 * No dependencies beyond Node itself.
 */
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

let passed = 0;
async function test(name, fn) {
  await fn();
  passed += 1;
  console.log(`ok - ${name}`);
}

function makeFakeApi(hostCalls, firefox) {
  const port = {
    postMessage() {},
    disconnect() {},
    onMessage: { addListener() {} },
    onDisconnect: { addListener() {} },
  };
  const listeners = [];
  const api = {
    storage: {
      local: {
        get: async () => ({}),
        set: async () => {},
      },
    },
    downloads: {
      onCreated: { addListener: (fn) => listeners.push(fn) },
      pause: async () => {},
      resume: async () => {},
      cancel: async () => {},
      search: async () => [],
      erase: async () => {},
      download: async () => 99,
    },
    runtime: {
      lastError: undefined,
      id: firefox ? "udm-handoff@cino.pw" : "nfhcgogppockkjnjkhpmncjakpanjebl",
      connectNative: (name) => {
        hostCalls.push(name);
        return port;
      },
    },
  };
  if (firefox) {
    // The Firefox-only API the detection keys on.
    api.runtime.getBrowserInfo = async () => ({ name: "Firefox", version: "160.0" });
  }
  return { api, listeners };
}

const ITEM = {
  id: 7,
  url: "https://example.com/file.bin",
  filename: "file.bin",
  state: "in_progress",
  fileSize: 10,
};
const tick = () => new Promise((r) => setTimeout(r, 100));

(async () => {
  const read = (file) => fs.readFileSync(path.join(__dirname, "..", file), "utf8");

  // ---- Firefox model: no importScripts, one shared scope, and a
  // ---- `browser` global WITH runtime.getBrowserInfo.
  const fxHosts = [];
  const fx = makeFakeApi(fxHosts, true);
  const fxContext = vm.createContext({ browser: fx.api, setTimeout, clearTimeout, console, URL });
  const loadFx = (file) => vm.runInContext(read(file), fxContext, { filename: file });

  await test("the modeled Firefox scope has no importScripts", () => {
    assert.strictEqual(vm.runInContext("typeof importScripts", fxContext), "undefined");
  });

  await test("handoff.js loads in the Firefox scope", () => {
    loadFx("handoff.js");
  });

  await test("background.js loads after handoff.js without a lexical collision", () => {
    loadFx("background.js");
  });

  await test("UdmHandoff is defined on the shared global", () => {
    assert.ok(vm.runInContext("globalThis.UdmHandoff", fxContext));
  });

  await test("exactly one downloads.onCreated listener is registered (Firefox)", () => {
    assert.strictEqual(fx.listeners.length, 1);
    assert.strictEqual(typeof fx.listeners[0], "function");
  });

  await test("a download in Firefox connects to the Firefox native host", async () => {
    fx.listeners[0](ITEM);
    await tick();
    assert.deepStrictEqual(fxHosts, ["pw.cino.udm.handoff.firefox"]);
  });

  // ---- Chrome model: importScripts pulls handoff.js into the worker
  // ---- scope; BOTH `chrome` and a `browser` alias exist, but the alias
  // ---- has no runtime.getBrowserInfo.
  const crHosts = [];
  const cr = makeFakeApi(crHosts, false);
  const crContext = vm.createContext({ setTimeout, clearTimeout, console, URL });
  crContext.chrome = cr.api;
  crContext.browser = cr.api;
  crContext.importScripts = (file) =>
    vm.runInContext(read(file), crContext, { filename: file });

  await test("background.js loads in the Chrome worker scope (importScripts)", () => {
    vm.runInContext(read("background.js"), crContext, { filename: "background.js" });
  });

  await test("exactly one downloads.onCreated listener is registered (Chrome)", () => {
    assert.strictEqual(cr.listeners.length, 1);
    assert.strictEqual(typeof cr.listeners[0], "function");
  });

  await test("a download in Chrome connects to the Chrome native host even though a `browser` global exists", async () => {
    cr.listeners[0](ITEM);
    await tick();
    assert.deepStrictEqual(crHosts, ["pw.cino.udm.handoff.chrome"]);
  });

  // ---- fx.cancel confirmation polling (Chrome field case 2026-11-16):
  // ---- Chrome applies a cancel asynchronously; the state can still read
  // ---- "in_progress" when the cancel request resolves. fx.cancel must
  // ---- poll for the transition instead of checking exactly once.
  function cancelContext(flip) {
    const hosts = [];
    const fake = makeFakeApi(hosts, false);
    let state = "in_progress";
    fake.api.downloads.cancel = async () => {
      if (flip) setTimeout(() => { state = flip; }, 250);
    };
    fake.api.downloads.search = async () => [{ id: ITEM.id, state }];
    const ctx = vm.createContext({ setTimeout, clearTimeout, console, URL });
    ctx.chrome = fake.api;
    ctx.browser = fake.api;
    ctx.importScripts = (file) => vm.runInContext(read(file), ctx, { filename: file });
    vm.runInContext(read("background.js"), ctx, { filename: "background.js" });
    return ctx;
  }

  await test("fx.cancel confirms once Chrome lands the interrupted state", async () => {
    const ctx = cancelContext("interrupted");
    const confirmed = await vm.runInContext("fx.cancel(7)", ctx);
    assert.strictEqual(confirmed, true);
  });

  await test("fx.cancel refuses when the download completes instead", async () => {
    const ctx = cancelContext("complete");
    const confirmed = await vm.runInContext("fx.cancel(7)", ctx);
    assert.strictEqual(confirmed, false);
  });

  await test("fx.cancel gives up when the state never settles", async () => {
    const ctx = cancelContext(null);
    const confirmed = await vm.runInContext("fx.cancel(7)", ctx);
    assert.strictEqual(confirmed, false);
  });

  // ---- re-issue guard (0.2.3): a download this layer re-issued after
  // ---- a cancelled handoff must not start a second handoff, or a
  // ---- down coordinator would loop cancel -> re-issue forever.
  await test("startup resumes a prepared pending record by query and does not cancel", async () => {
    const hosts = [];
    const sent = [];
    const cancels = [];
    const downloads = [];
    const fake = makeFakeApi(hosts, false);
    fake.api.storage.local.get = async (key) => {
      if (key === "udmHandoffPending") {
        return {
          udmHandoffPending: {
            "tx-boot": {
              tx: "tx-boot",
              stage: "prepared",
              intent: {
                url: "https://example.com/file.bin",
                method: "GET",
                filename_hint: "file.bin",
                size_hint: 10,
                browser_download_id: "7",
                dest_dir: null,
              },
            },
          },
        };
      }
      return {};
    };
    fake.api.downloads.cancel = async () => {
      cancels.push("cancel");
    };
    fake.api.downloads.download = async (opts) => {
      downloads.push(opts && opts.url);
      return 99;
    };
    fake.api.runtime.connectNative = (name) => {
      hosts.push(name);
      return {
        postMessage(message) {
          sent.push(message);
        },
        disconnect() {},
        onMessage: { addListener() {} },
        onDisconnect: { addListener() {} },
      };
    };
    const ctx = vm.createContext({ setTimeout, clearTimeout, console, URL });
    ctx.chrome = fake.api;
    ctx.browser = fake.api;
    ctx.importScripts = (file) => vm.runInContext(read(file), ctx, { filename: file });
    vm.runInContext(read("background.js"), ctx, { filename: "background.js" });
    for (let i = 0; i < 20 && sent.length === 0; i++) await tick();
    assert.strictEqual(sent.length, 1);
    assert.strictEqual(sent[0].op, "query");
    assert.strictEqual(sent[0].tx, "tx-boot");
    assert.deepStrictEqual(hosts, ["pw.cino.udm.handoff.chrome"]);
    assert.deepStrictEqual(cancels, []);
    assert.deepStrictEqual(downloads, []);
  });

  await test("startup migration drops handed-off rows and leaves the live pending transaction", async () => {
    const hosts = [];
    const sent = [];
    const sets = [];
    const error = { tx: "e", name: "bad.pdf", status: "recovery-required", at: 2 };
    const discarded = { tx: "d", name: "no.pdf", status: "discarded", at: 3 };
    const store = {
      udmSettings: { enabled: true, exclusions: ["*.example.com"] },
      udmHandoffLog: [
        { tx: "s", name: "ok.pdf", status: "handed-off", task_ref: "1", at: 1 },
        error,
        discarded,
      ],
      udmHandoffPending: {
        "tx-live": {
          tx: "tx-live",
          stage: "prepared",
          task_ref: null,
          reissued: false,
          intent: {
            url: "https://example.com/live.bin",
            method: "GET",
            filename_hint: "live.bin",
            size_hint: 10,
            browser_download_id: "7",
            dest_dir: null,
          },
        },
      },
    };
    const pendingBefore = JSON.parse(JSON.stringify(store.udmHandoffPending));
    const settingsBefore = JSON.parse(JSON.stringify(store.udmSettings));
    const fake = makeFakeApi(hosts, false);
    fake.api.storage.local.get = async (key) => {
      const keys = Array.isArray(key) ? key : [key];
      const out = {};
      for (const k of keys) {
        if (Object.prototype.hasOwnProperty.call(store, k)) out[k] = store[k];
      }
      return out;
    };
    fake.api.storage.local.set = async (obj) => {
      sets.push(JSON.parse(JSON.stringify(obj)));
      for (const [k, v] of Object.entries(obj)) store[k] = JSON.parse(JSON.stringify(v));
    };
    fake.api.runtime.connectNative = (name) => {
      hosts.push(name);
      return {
        postMessage(message) {
          sent.push(message);
        },
        disconnect() {},
        onMessage: { addListener() {} },
        onDisconnect: { addListener() {} },
      };
    };
    const ctx = vm.createContext({ setTimeout, clearTimeout, console, URL });
    ctx.chrome = fake.api;
    ctx.browser = fake.api;
    ctx.importScripts = (file) => vm.runInContext(read(file), ctx, { filename: file });
    vm.runInContext(read("background.js"), ctx, { filename: "background.js" });
    for (let i = 0; i < 20 && sent.length === 0; i++) await tick();
    assert.strictEqual(sent.length, 1);
    assert.strictEqual(sent[0].op, "query");
    assert.strictEqual(sent[0].tx, "tx-live");
    const logWrites = sets.filter((s) => Object.prototype.hasOwnProperty.call(s, "udmHandoffLog"));
    assert.strictEqual(logWrites.length, 1);
    assert.deepStrictEqual(
      logWrites[0].udmHandoffLog.map((e) => e.tx),
      ["e", "d"],
    );
    assert.deepStrictEqual(logWrites[0].udmHandoffLog[0], error);
    assert.deepStrictEqual(logWrites[0].udmHandoffLog[1], discarded);
    assert.ok(sets.every((s) => !Object.prototype.hasOwnProperty.call(s, "udmSettings")));
    assert.ok(sets.every((s) => !Object.prototype.hasOwnProperty.call(s, "udmHandoffPending")));
    assert.deepStrictEqual(store.udmHandoffPending, pendingBefore);
    assert.deepStrictEqual(store.udmSettings, settingsBefore);
    assert.ok(Array.isArray(store.udmHandoffLog));

    await vm.runInContext(
      "fx.record({tx:'s2', name:'again.pdf', status:'handed-off', task_ref:'9', at:9})",
      ctx,
    );
    assert.deepStrictEqual(
      store.udmHandoffLog.map((e) => e.status),
      ["recovery-required", "discarded"],
    );
    await vm.runInContext(
      "fx.record({tx:'k', name:'kept.pdf', status:'kept-by-browser', at:10})",
      ctx,
    );
    assert.deepStrictEqual(
      store.udmHandoffLog.map((e) => [e.tx, e.status]),
      [
        ["k", "kept-by-browser"],
        ["e", "recovery-required"],
        ["d", "discarded"],
      ],
    );
    assert.deepStrictEqual(store.udmHandoffPending, pendingBefore);
    assert.deepStrictEqual(store.udmSettings, settingsBefore);
  });

  await test("recording a success does not create udmHandoffLog", async () => {
    const sets = [];
    const store = {};
    const fake = makeFakeApi([], false);
    fake.api.storage.local.get = async (key) => {
      const keys = Array.isArray(key) ? key : [key];
      const out = {};
      for (const k of keys) {
        if (Object.prototype.hasOwnProperty.call(store, k)) out[k] = store[k];
      }
      return out;
    };
    fake.api.storage.local.set = async (obj) => {
      sets.push(JSON.parse(JSON.stringify(obj)));
      for (const [k, v] of Object.entries(obj)) store[k] = JSON.parse(JSON.stringify(v));
    };
    const ctx = vm.createContext({ setTimeout, clearTimeout, console, URL });
    ctx.chrome = fake.api;
    ctx.browser = fake.api;
    ctx.importScripts = (file) => vm.runInContext(read(file), ctx, { filename: file });
    vm.runInContext(read("background.js"), ctx, { filename: "background.js" });
    await tick();
    await vm.runInContext("fx.record({tx:'s', name:'a.pdf', status:'handed-off', task_ref:'1', at:1})", ctx);
    assert.ok(!Object.prototype.hasOwnProperty.call(store, "udmHandoffLog"));
    assert.ok(sets.every((s) => !Object.prototype.hasOwnProperty.call(s, "udmHandoffLog")));
    await vm.runInContext("fx.record({tx:'d', name:'no.pdf', status:'discarded', at:2})", ctx);
    assert.deepStrictEqual(store.udmHandoffLog, [{ tx: "d", name: "no.pdf", status: "discarded", at: 2 }]);
  });

  await test("a re-issued download is skipped once; a fresh one is not", async () => {
    const hosts = [];
    const fake = makeFakeApi(hosts, false);
    const ctx = vm.createContext({ setTimeout, clearTimeout, console, URL });
    ctx.chrome = fake.api;
    ctx.browser = fake.api;
    ctx.importScripts = (file) => vm.runInContext(read(file), ctx, { filename: file });
    vm.runInContext(read("background.js"), ctx, { filename: "background.js" });
    await vm.runInContext("fx.download('https://example.com/file.bin')", ctx);
    fake.listeners[0](ITEM);
    await tick();
    assert.deepStrictEqual(hosts, []);
    fake.listeners[0](Object.assign({}, ITEM, { id: 8, url: "https://example.com/other.bin" }));
    await tick();
    assert.deepStrictEqual(hosts, ["pw.cino.udm.handoff.chrome"]);
  });

  console.log(`\n${passed} tests passed`);
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
