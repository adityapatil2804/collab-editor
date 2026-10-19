// Offline persistence in IndexedDB.
//
// Every op is its own record, keyed [doc, counter, replica], written as soon
// as it exists (local keystroke or remote op). So:
// - saving is O(ops written), not O(document) per keystroke;
// - nothing is lost if the tab closes right after typing (no debounce window);
// - two tabs writing the same document never overwrite each other: records
//   are unique per op id, and writing one twice is harmless.

import type { Op } from "./crdt.js";
import { JsonOp, opFromJson, opToJson } from "./protocol.js";

const DB = "collab-editor";
const STORE = "ops";

let dbPromise: Promise<IDBDatabase> | null = null;

function open(): Promise<IDBDatabase> {
  dbPromise ??= new Promise((resolve, reject) => {
    const req = indexedDB.open(DB, 2);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (db.objectStoreNames.contains("docs")) db.deleteObjectStore("docs"); // v1 layout
      if (!db.objectStoreNames.contains(STORE)) db.createObjectStore(STORE);
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => {
      dbPromise = null;
      reject(req.error);
    };
  });
  return dbPromise;
}

/** All of a document's keys: [doc, <any number>, <any number>]. */
const docRange = (name: string) => IDBKeyRange.bound([name], [name, []]);

export async function loadOps(name: string): Promise<Op[]> {
  try {
    const db = await open();
    return await new Promise((resolve, reject) => {
      const req = db.transaction(STORE).objectStore(STORE).getAll(docRange(name));
      req.onsuccess = () => resolve((req.result as JsonOp[]).map(opFromJson));
      req.onerror = () => reject(req.error);
    });
  } catch {
    return []; // private mode / storage disabled: run online-only
  }
}

export async function saveOps(name: string, ops: Op[]): Promise<void> {
  if (!ops.length) return;
  try {
    const db = await open();
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction(STORE, "readwrite");
      const store = tx.objectStore(STORE);
      for (const op of ops) store.put(opToJson(op), [name, op.id.counter, op.id.replica]);
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error);
    });
  } catch (e) {
    console.warn("offline save failed", e);
  }
}
