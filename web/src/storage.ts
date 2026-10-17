// Offline persistence in IndexedDB.
//
// Each document's ops are stored as one record. Saves *merge* with what is
// already stored (union by op id inside one transaction), so two tabs editing
// the same document offline never overwrite each other's work.

import type { Op } from "./crdt.js";
import { JsonOp, opFromJson, opToJson } from "./protocol.js";

const DB = "collab-editor";
const STORE = "docs";

function open(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB, 1);
    req.onupgradeneeded = () => req.result.createObjectStore(STORE, { keyPath: "name" });
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

interface Record {
  name: string;
  ops: JsonOp[];
}

const key = (j: JsonOp) => `${j.id[0]}:${j.id[1]}`;

export async function loadOps(name: string): Promise<Op[]> {
  try {
    const db = await open();
    return await new Promise((resolve, reject) => {
      const req = db.transaction(STORE).objectStore(STORE).get(name);
      req.onsuccess = () => resolve(((req.result as Record | undefined)?.ops ?? []).map(opFromJson));
      req.onerror = () => reject(req.error);
    });
  } catch {
    return []; // private mode / storage disabled: run online-only
  }
}

export async function saveOps(name: string, ops: Op[]): Promise<void> {
  try {
    const db = await open();
    await new Promise<void>((resolve, reject) => {
      const tx = db.transaction(STORE, "readwrite");
      const store = tx.objectStore(STORE);
      const req = store.get(name);
      req.onsuccess = () => {
        const existing = (req.result as Record | undefined)?.ops ?? [];
        const seen = new Set(existing.map(key));
        const merged = existing.slice();
        for (const op of ops) {
          const j = opToJson(op);
          if (!seen.has(key(j))) {
            seen.add(key(j));
            merged.push(j);
          }
        }
        store.put({ name, ops: merged } satisfies Record);
      };
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error);
    });
  } catch (e) {
    console.warn("offline save failed", e);
  }
}
