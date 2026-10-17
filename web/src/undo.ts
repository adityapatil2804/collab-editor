// Undo/redo of *your own* edits in a shared document.
//
// Undo never rewinds other people's work. It applies new inverse operations:
//   undo an insert -> delete those exact characters (by id), if still present
//   undo a delete  -> re-insert the characters right after their tombstones,
//                     so they reappear where they were even if others edited nearby
// Redo inverts the inverse the same way.

import type { Doc, Op, Patch } from "./crdt.js";

export interface Step {
  ops: Op[];
  patches: Patch[];
}

const GROUP_MS = 700;

export class UndoManager {
  private undoStack: Op[][] = [];
  private redoStack: Op[][] = [];
  private lastTime = 0;
  private lastKind = "";

  /** Record a local edit. Consecutive edits of the same kind are grouped. */
  record(ops: Op[], now = Date.now()): void {
    for (const op of ops) {
      const top = this.undoStack[this.undoStack.length - 1];
      if (top && op.kind.type === this.lastKind && now - this.lastTime < GROUP_MS) {
        top.push(op);
      } else {
        this.undoStack.push([op]);
      }
      this.lastKind = op.kind.type;
      this.lastTime = now;
    }
    if (ops.length) this.redoStack = [];
  }

  /** End the current group (e.g. after the caret jumps). */
  breakGroup(): void {
    this.lastKind = "";
  }

  get canUndo(): boolean {
    return this.undoStack.length > 0;
  }

  get canRedo(): boolean {
    return this.redoStack.length > 0;
  }

  undo(doc: Doc): Step | null {
    return this.step(doc, this.undoStack, this.redoStack);
  }

  redo(doc: Doc): Step | null {
    return this.step(doc, this.redoStack, this.undoStack);
  }

  private step(doc: Doc, from: Op[][], to: Op[][]): Step | null {
    this.breakGroup();
    // Skip groups that became no-ops (e.g. someone else already deleted the text).
    while (from.length) {
      const inv = invert(doc, from.pop()!);
      if (inv.ops.length) {
        to.push(inv.ops);
        return inv;
      }
    }
    return null;
  }
}

function invert(doc: Doc, group: Op[]): Step {
  const ops: Op[] = [];
  const patches: Patch[] = [];
  for (let i = group.length - 1; i >= 0; i--) {
    const k = group[i].kind;
    if (k.type === "ins") {
      const r = doc.deleteById(group[i].id);
      if (r) {
        ops.push(r.op);
        patches.push(r.patch);
      }
    } else {
      const ch = doc.charOf(k.target);
      if (ch !== undefined) {
        const r = doc.insertAfterId(k.target, ch);
        ops.push(r.op);
        patches.push(r.patch);
      }
    }
  }
  return { ops, patches };
}
