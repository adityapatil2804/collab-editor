// End-to-end: real TypeScript clients against a running server.
//
//   SERVER=127.0.0.1:8080 node test/e2e.mjs

import assert from "node:assert/strict";
import { Doc, randomReplica } from "../dist/crdt.js";
import { SyncClient } from "../dist/sync.js";
import { UndoManager } from "../dist/undo.js";

const SERVER = process.env.SERVER ?? "127.0.0.1:8080";
const URL = `ws://${SERVER}/ws`;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function until(what, cond, ms = 10_000) {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error(`timed out waiting for: ${what}`);
    await sleep(20);
  }
}

function client(docName) {
  const doc = new Doc(randomReplica());
  const sync = new SyncClient(doc, URL, docName);
  sync.connect();
  return { doc, sync, undo: new UndoManager() };
}

function edit(c, ops) {
  c.undo.record(ops);
  c.sync.sendOps(ops);
}

const CHARS = Array.from("abcdefgh😀é");
const total = (doc) => [...doc.vv.values()].reduce((a, b) => a + b, 0);

async function serverText(name) {
  return (await fetch(`http://${SERVER}/export/${name}`)).text();
}

async function concurrentWithOfflineClient() {
  const name = `e2e-${Date.now()}`;
  const cs = Array.from({ length: 4 }, () => client(name));
  await until("all online", () => cs.every((c) => c.sync.status === "online"));

  let sent = 0;
  const editsEach = 60;
  await Promise.all(
    cs.map(async (c, i) => {
      for (let k = 0; k < editsEach; k++) {
        if (i === 2 && k === 15) c.sync.setOffline(true); // drop off mid-session
        if (i === 2 && k === 45) c.sync.setOffline(false); // come back
        const len = c.doc.length;
        const op =
          len > 0 && Math.random() < 0.25
            ? c.doc.delete(Math.floor(Math.random() * len))
            : c.doc.insert(Math.floor(Math.random() * (len + 1)), CHARS[k % CHARS.length]);
        edit(c, [op]);
        sent++;
        await sleep(Math.random() * 15);
      }
    }),
  );

  await until("all replicas have every op", () => cs.every((c) => total(c.doc) === sent));
  const text = cs[0].doc.text();
  for (const c of cs) assert.equal(c.doc.text(), text, "clients diverged");
  let server = "";
  for (let i = 0; i < 50 && server !== text; i++) {
    server = await serverText(name);
    if (server !== text) await sleep(50);
  }
  assert.equal(server, text, "server diverged");

  // A brand-new client gets the whole document from the server.
  const late = client(name);
  await until("late joiner synced", () => late.doc.text() === text);

  for (const c of [...cs, late]) c.sync.close();
  console.log(`ok  4 clients (1 offline mid-session) + late joiner converged on ${[...text].length} chars`);
}

async function undoOnlyUndoesYourOwnEdits() {
  const name = `undo-${Date.now()}`;
  const a = client(name);
  const b = client(name);
  await until("online", () => a.sync.status === "online" && b.sync.status === "online");

  edit(a, [..."hello"].map((ch, i) => a.doc.insert(i, ch)));
  await until("b sees hello", () => b.doc.text() === "hello");
  edit(b, [..." world"].map((ch, i) => b.doc.insert(5 + i, ch)));
  await until("a sees hello world", () => a.doc.text() === "hello world");

  a.sync.sendOps(a.undo.undo(a.doc).ops);
  await until("undo propagates", () => a.doc.text() === " world" && b.doc.text() === " world");

  a.sync.sendOps(a.undo.redo(a.doc).ops);
  await until("redo propagates", () => b.doc.text() === "hello world" && a.doc.text() === "hello world");
  assert.equal(await serverText(name), "hello world");

  a.sync.close();
  b.sync.close();
  console.log("ok  undo/redo affects only the undoing user's text, on every replica");
}

try {
  await concurrentWithOfflineClient();
  await undoOnlyUndoesYourOwnEdits();
  process.exit(0);
} catch (e) {
  console.error("FAIL", e);
  process.exit(1);
}
