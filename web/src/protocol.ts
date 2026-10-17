// Wire format; mirrors crates/collab-server/src/protocol.rs.

import type { Op, OpId, VersionVector } from "./crdt.js";

type JsonId = [number, number];
export type JsonOp =
  | { id: JsonId; s: number; i: { l: JsonId | null; r: JsonId | null; c: string } }
  | { id: JsonId; s: number; d: JsonId };

const idTo = (id: OpId): JsonId => [id.counter, id.replica];
const idFrom = (j: JsonId): OpId => ({ counter: j[0], replica: j[1] });
const optTo = (id: OpId | null): JsonId | null => (id ? idTo(id) : null);
const optFrom = (j: JsonId | null | undefined): OpId | null => (j ? idFrom(j) : null);

export function opToJson(op: Op): JsonOp {
  const k = op.kind;
  return k.type === "ins"
    ? { id: idTo(op.id), s: op.seq, i: { l: optTo(k.left), r: optTo(k.right), c: k.ch } }
    : { id: idTo(op.id), s: op.seq, d: idTo(k.target) };
}

export function opFromJson(j: JsonOp): Op {
  if ("i" in j) {
    return {
      id: idFrom(j.id),
      seq: j.s,
      kind: { type: "ins", left: optFrom(j.i.l), right: optFrom(j.i.r), ch: j.i.c },
    };
  }
  return { id: idFrom(j.id), seq: j.s, kind: { type: "del", target: idFrom(j.d) } };
}

export function vvToJson(vv: VersionVector): Record<string, number> {
  const o: Record<string, number> = {};
  for (const [r, s] of vv) o[String(r)] = s;
  return o;
}

export function vvFromJson(o: Record<string, number> | undefined): VersionVector {
  const vv: VersionVector = new Map();
  for (const [r, s] of Object.entries(o ?? {})) vv.set(Number(r), s);
  return vv;
}

export const idToJson = optTo;
export const idFromJson = optFrom;

export interface PresenceMsg {
  t: "presence";
  replica: number;
  name: string;
  color: string;
  cursor: JsonId | null;
}

export type ServerMsg =
  | { t: "welcome"; ops: JsonOp[]; vv: Record<string, number>; peers: PresenceMsg[] }
  | { t: "ops"; ops: JsonOp[] }
  | PresenceMsg
  | { t: "leave"; replica: number }
  | { t: "error"; msg: string };
