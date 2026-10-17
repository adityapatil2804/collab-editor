// Cross-language consistency between the TypeScript and Rust CRDTs.
//
//   node test/crosslang.mjs check <rust-trace.json>  TS replays Rust-authored ops (shuffled)
//   node test/crosslang.mjs gen <out.json>           TS authors ops (incl. undo/redo) for Rust to check

import fs from "node:fs";
import { Doc } from "../dist/crdt.js";
import { opFromJson, opToJson } from "../dist/protocol.js";
import { UndoManager } from "../dist/undo.js";

function rng(seed) {
  let x = BigInt(seed) * 0x9e3779b97f4a7c15n | 1n;
  const M = (1n << 64n) - 1n;
  return (n) => {
    x ^= (x << 13n) & M;
    x ^= x >> 7n;
    x ^= (x << 17n) & M;
    return Number(x % BigInt(Math.max(1, n)));
  };
}

function sync(a, b) {
  const toB = a.opsSince(b.vv);
  const toA = b.opsSince(a.vv);
  for (const op of toB) b.apply(op);
  for (const op of toA) a.apply(op);
}

const ALPHABET = ["a", "b", "c", " ", "\n", "é", "ß", "中", "😀", "🚀", '"', "\\"];

function check(file) {
  const { cases } = JSON.parse(fs.readFileSync(file, "utf8"));
  cases.forEach((c, i) => {
    const ops = c.ops.map(opFromJson);
    const r = rng(i + 1);
    for (let k = ops.length - 1; k > 0; k--) {
      const j = r(k + 1);
      [ops[k], ops[j]] = [ops[j], ops[k]];
    }
    const doc = new Doc(1);
    for (const op of ops) doc.apply(op);
    if (doc.pendingCount !== 0) throw new Error(`case ${i}: ops stuck`);
    if (doc.text() !== c.text) {
      throw new Error(`case ${i}: TS=${JSON.stringify(doc.text())} Rust=${JSON.stringify(c.text)}`);
    }
  });
  console.log(`TypeScript agrees with Rust on ${cases.length} shuffled cases`);
}

function gen(file) {
  const cases = [];
  for (let seed = 1; seed <= 200; seed++) {
    const r = rng(seed * 31);
    const n = 2 + r(3);
    const docs = Array.from({ length: n }, (_, i) => new Doc(500 + i * 1013));
    const undos = docs.map(() => new UndoManager());
    let t = 0;
    for (let step = 0; step < 150; step++) {
      const i = r(n);
      const d = docs[i];
      const roll = r(100);
      t += 300 * r(4); // vary timing so undo groups vary
      if (roll < 8) {
        undos[i].undo(d);
      } else if (roll < 12) {
        undos[i].redo(d);
      } else if (d.length > 0 && roll < 35) {
        undos[i].record([d.delete(r(d.length))], t);
      } else {
        undos[i].record([d.insert(r(d.length + 1), ALPHABET[r(ALPHABET.length)])], t);
      }
      if (r(10) === 0) {
        const j = r(n);
        if (j !== i) sync(docs[i], docs[j]);
      }
    }
    for (let k = 0; k < 2; k++) for (let i = 1; i < n; i++) sync(docs[0], docs[i]);
    for (const d of docs) {
      if (d.text() !== docs[0].text()) throw new Error(`seed ${seed}: TS replicas diverged`);
    }
    cases.push({ ops: docs[0].log.map(opToJson), text: docs[0].text() });
  }
  fs.writeFileSync(file, JSON.stringify({ cases }));
  console.log(`wrote ${cases.length} TypeScript-generated cases (with undo/redo) to ${file}`);
}

const [mode, file] = process.argv.slice(2);
if (mode === "check") check(file);
else if (mode === "gen") gen(file);
else {
  console.error("usage: crosslang.mjs check|gen <file>");
  process.exit(2);
}
