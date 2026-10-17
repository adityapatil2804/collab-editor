// Editor binding tests without a browser: a minimal DOM shim stands in for the
// textarea, and random "user typing" is simulated the way a browser reports it
// (new value + caret, then an `input` event).
//
//   node test/editor.mjs

import assert from "node:assert/strict";

// ---- DOM shim -------------------------------------------------------------
globalThis.ResizeObserver = class { observe() {} };
globalThis.requestAnimationFrame = (f) => setTimeout(f, 0);
globalThis.document = { activeElement: null };

class FakeTextarea {
  value = "";
  selectionStart = 0;
  selectionEnd = 0;
  scrollTop = 0;
  clientWidth = 600;
  clientHeight = 400;
  listeners = {};
  addEventListener(t, f) {
    (this.listeners[t] ??= []).push(f);
  }
  fire(t, e = {}) {
    for (const f of this.listeners[t] ?? []) f({ preventDefault() {}, ...e });
  }
  setSelectionRange(a, b) {
    this.selectionStart = a;
    this.selectionEnd = b;
  }
  /** Simulate the user replacing the selection with `text` (typing, paste, delete). */
  type(text, backspace = false) {
    let { selectionStart: s, selectionEnd: e } = this;
    if (backspace && s === e && s > 0) {
      s -= this.value.codePointAt(s - 2) > 0xffff ? 2 : 1; // delete one code point
    }
    this.value = this.value.slice(0, s) + text + this.value.slice(e);
    this.selectionStart = this.selectionEnd = s + text.length;
    this.fire("input");
  }
}

const { Doc } = await import("../dist/crdt.js");
const { Editor } = await import("../dist/editor.js");

function rng(seed) {
  let x = seed >>> 0 || 1;
  return (n) => {
    x ^= x << 13;
    x ^= x >>> 17;
    x ^= x << 5;
    return (x >>> 0) % Math.max(1, n);
  };
}

/** Random caret/selection on code-point boundaries. */
function select(ta, r) {
  const cps = Array.from(ta.value);
  const a = r(cps.length + 1);
  const b = r(4) === 0 ? Math.min(cps.length, a + r(5)) : a;
  const u = (cp) => cps.slice(0, cp).join("").length;
  ta.setSelectionRange(u(a), u(b));
}

const WORDS = ["a", "b", "x", " ", "\n", "é", "😀", "hello", "ab😀", "aa"];

function mkEditor(replica, sent) {
  const ta = new FakeTextarea();
  const doc = new Doc(replica);
  const ed = new Editor(ta, { style: {}, innerHTML: "", scrollTop: 0 }, doc, {
    localOps: (ops) => sent.push(...ops),
    caret() {},
  });
  return { ta, doc, ed };
}

// 1. Single user: every simulated edit must leave doc.text() === textarea value.
for (let seed = 1; seed <= 200; seed++) {
  const r = rng(seed);
  const { ta, doc } = mkEditor(1, []);
  globalThis.document.activeElement = ta;
  for (let i = 0; i < 80; i++) {
    select(ta, r);
    if (r(4) === 0) ta.type("", true);
    else ta.type(WORDS[r(WORDS.length)]);
    assert.equal(doc.text(), ta.value, `seed ${seed} step ${i}`);
  }
}
console.log("ok  textarea diffing matches the document (200 seeds, emoji, selections, backspace)");

// 2. Two editors exchanging ops: values converge and remote edits shift the caret.
for (let seed = 1; seed <= 200; seed++) {
  const r = rng(seed * 7);
  const outA = [];
  const outB = [];
  const A = mkEditor(10, outA);
  const B = mkEditor(20, outB);
  const deliver = (from, to) => {
    const patches = [];
    for (const op of from.splice(0)) to.doc.applyCollect(op, patches);
    to.ed.applyRemote(patches);
  };
  for (let i = 0; i < 60; i++) {
    const me = r(2) ? A : B;
    globalThis.document.activeElement = me.ta;
    select(me.ta, r);
    if (r(4) === 0) me.ta.type("", true);
    else me.ta.type(WORDS[r(WORDS.length)]);
    if (r(3) === 0) deliver(outA, B);
    if (r(3) === 0) deliver(outB, A);
  }
  deliver(outA, B);
  deliver(outB, A);
  assert.equal(A.ta.value, B.ta.value, `seed ${seed}: editors diverged`);
  assert.equal(A.ta.value, A.doc.text());
}

// Caret stays on the same character when someone types before it.
{
  const outA = [];
  const outB = [];
  const A = mkEditor(1, outA);
  const B = mkEditor(2, outB);
  globalThis.document.activeElement = A.ta;
  A.ta.type("world");
  let p = [];
  for (const op of outA.splice(0)) B.doc.applyCollect(op, p);
  B.ed.applyRemote(p);
  A.ta.setSelectionRange(2, 2); // wo|rld
  globalThis.document.activeElement = B.ta;
  B.ta.setSelectionRange(0, 0);
  B.ta.type("hello ");
  globalThis.document.activeElement = A.ta;
  p = [];
  for (const op of outB.splice(0)) A.doc.applyCollect(op, p);
  A.ed.applyRemote(p);
  assert.equal(A.ta.value, "hello world");
  assert.equal(A.ta.selectionStart, 8, "caret should still be between 'wo' and 'rld'");
}
console.log("ok  two editors converge and remote edits keep the local caret in place");
process.exit(0);
