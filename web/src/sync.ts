// Network sync for one document. Works in browsers and in Node 22+ (global WebSocket).
//
// Reconnect protocol (no acks needed):
//   1. client -> hello {replica, vv}
//   2. server -> welcome {ops the client lacks, server vv}
//   3. client -> ops the server lacks (doc.opsSince(server vv))
// Anything sent while the socket was dying is simply re-derived in step 3.

import { Doc, Op, OpId, Patch } from "./crdt.js";
import {
  idFromJson,
  idToJson,
  opFromJson,
  opToJson,
  PresenceMsg,
  ServerMsg,
  vvFromJson,
  vvToJson,
} from "./protocol.js";

export type Status = "connecting" | "online" | "offline";

export interface Presence {
  replica: number;
  name: string;
  color: string;
  cursor: OpId | null;
}

export interface SyncEvents {
  /** Remote ops were applied; `patches` describe the visible text changes. */
  remote?(patches: Patch[], ops: Op[]): void;
  status?(s: Status): void;
  presence?(p: Presence): void;
  leave?(replica: number): void;
  error?(msg: string): void;
}

export class SyncClient {
  status: Status = "offline";
  private ws: WebSocket | null = null;
  private wantOffline = false;
  private retries = 0;
  private timer: ReturnType<typeof setTimeout> | null = null;
  private me: Omit<Presence, "replica"> | null = null;

  constructor(
    readonly doc: Doc,
    readonly url: string,
    readonly docName: string,
    private ev: SyncEvents = {},
  ) {}

  connect(): void {
    if (this.ws || this.wantOffline) return;
    this.setStatus("connecting");
    const ws = new WebSocket(this.url);
    this.ws = ws;
    ws.onopen = () => {
      ws.send(
        JSON.stringify({
          t: "hello",
          doc: this.docName,
          replica: this.doc.replica,
          vv: vvToJson(this.doc.vv),
        }),
      );
    };
    ws.onmessage = (e) => this.onMessage(JSON.parse(String(e.data)) as ServerMsg);
    ws.onclose = () => {
      if (this.ws !== ws) return;
      this.ws = null;
      this.setStatus("offline");
      if (!this.wantOffline) {
        // Exponential backoff with jitter, capped at 10 s.
        const delay = Math.min(10_000, 500 * 2 ** this.retries) * (0.5 + Math.random() / 2);
        this.retries++;
        this.timer = setTimeout(() => {
          this.timer = null;
          this.connect();
        }, delay);
      }
    };
    ws.onerror = () => ws.close();
  }

  /** Simulate losing the network (or restore it). */
  setOffline(offline: boolean): void {
    this.wantOffline = offline;
    if (offline) {
      if (this.timer) clearTimeout(this.timer);
      this.timer = null;
      const ws = this.ws;
      this.ws = null;
      ws?.close();
      this.setStatus("offline");
    } else {
      this.retries = 0;
      this.connect();
    }
  }

  /** The network came back: retry now instead of waiting out the backoff. */
  retryNow(): void {
    if (this.wantOffline || this.ws) return;
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    this.retries = 0;
    this.connect();
  }

  close(): void {
    this.setOffline(true);
  }

  /** Send local ops now if online; otherwise they go out on the next welcome. */
  sendOps(ops: Op[]): void {
    if (ops.length && this.status === "online" && this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify({ t: "ops", ops: ops.map(opToJson) }));
    }
  }

  sendPresence(p: Omit<Presence, "replica">): void {
    this.me = p;
    if (this.status === "online" && this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(
        JSON.stringify({ t: "presence", name: p.name, color: p.color, cursor: idToJson(p.cursor) }),
      );
    }
  }

  private onMessage(m: ServerMsg): void {
    switch (m.t) {
      case "welcome": {
        this.applyRemote(m.ops.map(opFromJson));
        const missing = this.doc.opsSince(vvFromJson(m.vv));
        this.retries = 0;
        this.setStatus("online");
        this.sendOps(missing);
        for (const p of m.peers) this.ev.presence?.(presenceOf(p));
        if (this.me) this.sendPresence(this.me);
        break;
      }
      case "ops":
        this.applyRemote(m.ops.map(opFromJson));
        break;
      case "presence":
        this.ev.presence?.(presenceOf(m));
        break;
      case "leave":
        this.ev.leave?.(m.replica);
        break;
      case "error":
        this.ev.error?.(m.msg);
        break;
    }
  }

  /** Apply ops from any source (server, another tab). */
  applyRemote(ops: Op[]): void {
    if (!ops.length) return;
    const patches: Patch[] = [];
    const fresh: Op[] = [];
    for (const op of ops) {
      if (this.doc.applyCollect(op, patches) !== "duplicate") fresh.push(op);
    }
    if (fresh.length) this.ev.remote?.(patches, fresh);
  }

  private setStatus(s: Status): void {
    if (s !== this.status) {
      this.status = s;
      this.ev.status?.(s);
    }
  }
}

function presenceOf(p: PresenceMsg): Presence {
  return { replica: p.replica, name: p.name, color: p.color, cursor: idFromJson(p.cursor) };
}
