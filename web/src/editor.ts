// Binds a <textarea> to a Doc.
//
// Local typing: after each input event, diff the old and new text (in code
// points, anchored at the caret) and turn the difference into insert/delete ops.
// Remote changes: replace the textarea value with doc.text() and shift the
// local selection using the patches, so your caret doesn't jump.
// Remote cursors are drawn on a transparent overlay that mirrors the textarea.

import type { Doc, Op, OpId, Patch } from "./crdt.js";
import { UndoManager } from "./undo.js";

export interface Peer {
  name: string;
  color: string;
  cursor: OpId | null;
}

export interface EditorHooks {
  localOps(ops: Op[]): void;
  caret(anchor: OpId | null): void;
}

const cpLen = (s: string) => {
  let n = 0;
  for (const _ of s) n++;
  return n;
};

/** UTF-16 offset -> code-point offset. */
const toCp = (s: string, u16: number) => cpLen(s.slice(0, u16));

/** Code-point offset -> UTF-16 offset. */
function toU16(s: string, cp: number): number {
  let u = 0;
  for (const ch of s) {
    if (cp-- <= 0) break;
    u += ch.length;
  }
  return u;
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]!);
}

export class Editor {
  readonly undo = new UndoManager();
  readonly peers = new Map<number, Peer>();
  private last: string;
  private lastAnchor: string | undefined;
  private raf = 0;

  constructor(
    private ta: HTMLTextAreaElement,
    private overlay: HTMLElement,
    private doc: Doc,
    private hooks: EditorHooks,
  ) {
    this.last = doc.text();
    ta.value = this.last;

    ta.addEventListener("beforeinput", (e) => {
      if (e.inputType === "historyUndo" || e.inputType === "historyRedo") {
        e.preventDefault();
        e.inputType === "historyUndo" ? this.doUndo() : this.doRedo();
      }
    });
    ta.addEventListener("keydown", (e) => {
      const mod = e.ctrlKey || e.metaKey;
      const key = e.key.toLowerCase();
      if (mod && key === "z") {
        e.preventDefault();
        e.shiftKey ? this.doRedo() : this.doUndo();
      } else if (mod && key === "y") {
        e.preventDefault();
        this.doRedo();
      } else if (e.key.startsWith("Arrow") || e.key === "Home" || e.key === "End") {
        this.undo.breakGroup();
      }
    });
    ta.addEventListener("input", () => this.onInput());
    for (const ev of ["select", "keyup", "mouseup", "focus"]) {
      ta.addEventListener(ev, () => this.reportCaret());
    }
    ta.addEventListener("mousedown", () => this.undo.breakGroup());
    ta.addEventListener("scroll", () => this.scheduleRender());
    new ResizeObserver(() => this.scheduleRender()).observe(ta);
  }

  // ------------------------------------------------------------ local edits

  private onInput(): void {
    const next = this.ta.value;
    const a = Array.from(this.last);
    const b = Array.from(next);
    const caret = toCp(next, this.ta.selectionEnd);

    // Common suffix, but never past the caret (the inserted text ends at the caret).
    let s = 0;
    const sMax = Math.min(a.length, b.length, b.length - caret);
    while (s < sMax && a[a.length - 1 - s] === b[b.length - 1 - s]) s++;
    let p = 0;
    const pMax = Math.min(a.length, b.length) - s;
    while (p < pMax && a[p] === b[p]) p++;

    const ops: Op[] = [];
    for (let k = 0; k < a.length - p - s; k++) ops.push(this.doc.delete(p));
    const inserted = b.slice(p, b.length - s);
    inserted.forEach((ch, k) => ops.push(this.doc.insert(p + k, ch)));

    this.last = next;
    if (this.doc.text() !== next) {
      // Should never happen; recover rather than diverge from the CRDT.
      console.error("editor out of sync; resetting from document");
      this.setValue(this.doc.text(), caret, caret);
    }
    this.undo.record(ops);
    this.hooks.localOps(ops);
    this.reportCaret();
    this.scheduleRender();
  }

  private doUndo(): void {
    this.applyStep(this.undo.undo(this.doc));
  }

  private doRedo(): void {
    this.applyStep(this.undo.redo(this.doc));
  }

  private applyStep(step: { ops: Op[]; patches: Patch[] } | null): void {
    if (!step) return;
    const lastPatch = step.patches[step.patches.length - 1];
    const caret = lastPatch.type === "ins" ? lastPatch.pos + 1 : lastPatch.pos;
    this.setValue(this.doc.text(), caret, caret);
    this.hooks.localOps(step.ops);
    this.reportCaret();
  }

  // ------------------------------------------------------------ remote edits

  /** Reflect remote changes, keeping the local selection anchored. */
  applyRemote(patches: Patch[]): void {
    if (!patches.length) return;
    const v = this.ta.value;
    let start = toCp(v, this.ta.selectionStart);
    let end = toCp(v, this.ta.selectionEnd);
    for (const p of patches) {
      const d = p.type === "ins" ? 1 : -1;
      if (p.pos < start) start += d;
      if (p.pos < end) end += d;
    }
    this.setValue(this.doc.text(), Math.max(0, start), Math.max(0, end));
  }

  private setValue(text: string, startCp: number, endCp: number): void {
    const scroll = this.ta.scrollTop;
    const focused = document.activeElement === this.ta;
    this.ta.value = text;
    this.last = text;
    if (focused) this.ta.setSelectionRange(toU16(text, startCp), toU16(text, endCp));
    this.ta.scrollTop = scroll;
    this.scheduleRender();
  }

  // ------------------------------------------------------------ cursors

  private reportCaret(): void {
    const anchor = this.doc.anchorAt(toCp(this.ta.value, this.ta.selectionEnd));
    const k = anchor ? `${anchor.counter}:${anchor.replica}` : "start";
    if (k !== this.lastAnchor) {
      this.lastAnchor = k;
      this.hooks.caret(anchor);
    }
  }

  setPeer(replica: number, p: Peer): void {
    this.peers.set(replica, p);
    this.scheduleRender();
  }

  removePeer(replica: number): void {
    this.peers.delete(replica);
    this.scheduleRender();
  }

  scheduleRender(): void {
    if (!this.raf) {
      this.raf = requestAnimationFrame(() => {
        this.raf = 0;
        this.render();
      });
    }
  }

  private render(): void {
    const text = Array.from(this.ta.value);
    const marks: { pos: number; p: Peer }[] = [];
    for (const p of this.peers.values()) {
      const pos = this.doc.resolveAnchor(p.cursor);
      if (pos !== undefined) marks.push({ pos: Math.min(pos, text.length), p });
    }
    marks.sort((x, y) => x.pos - y.pos);
    let html = "";
    let at = 0;
    for (const { pos, p } of marks) {
      html += escapeHtml(text.slice(at, pos).join(""));
      html +=
        `<span class="rc" style="--c:${escapeHtml(p.color)}">` +
        `<span class="rc-name">${escapeHtml(p.name)}</span></span>`;
      at = pos;
    }
    html += escapeHtml(text.slice(at).join("")) + "​";
    this.overlay.innerHTML = html;
    this.overlay.style.width = `${this.ta.clientWidth}px`;
    this.overlay.style.height = `${this.ta.clientHeight}px`;
    this.overlay.scrollTop = this.ta.scrollTop;
  }
}
