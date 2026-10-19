// App bootstrap: load offline state, bind the editor, connect, share across tabs.

import { Doc, randomReplica } from "./crdt.js";
import { Editor } from "./editor.js";
import { opFromJson, opToJson, type JsonOp } from "./protocol.js";
import { loadOps, saveOps } from "./storage.js";
import { SyncClient, type Status } from "./sync.js";

const COLORS = ["#d9480f", "#2b8a3e", "#1864ab", "#862e9c", "#c2255c", "#087f5b", "#5f3dc4"];

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

function docNameFromUrl(): string {
  const raw = new URLSearchParams(location.search).get("doc") ?? "welcome";
  return raw.replace(/[^A-Za-z0-9_-]/g, "").slice(0, 64) || "welcome";
}

function storedName(): string {
  let n = "";
  try {
    n = localStorage.getItem("collab-name") ?? "";
  } catch {}
  return n || `Guest ${Math.floor(Math.random() * 900 + 100)}`;
}

async function main() {
  const docName = docNameFromUrl();
  document.title = `${docName} · Collab`;
  $("doc-name").textContent = docName;

  // Every page load is a fresh replica: two tabs must never share a replica id.
  const doc = new Doc(randomReplica());
  for (const op of await loadOps(docName)) doc.apply(op);

  const me = { name: storedName(), color: COLORS[doc.replica % COLORS.length] };
  const nameInput = $<HTMLInputElement>("name");
  nameInput.value = me.name;

  let saveTimer = 0;
  const persist = () => {
    clearTimeout(saveTimer);
    saveTimer = window.setTimeout(() => saveOps(docName, doc.log), 300);
  };

  const tabs = "BroadcastChannel" in window ? new BroadcastChannel(`collab:${docName}`) : null;

  const editor: Editor = new Editor($("editor"), $("overlay"), doc, {
    localOps(ops) {
      sync.sendOps(ops);
      tabs?.postMessage(ops.map(opToJson));
      persist();
      refreshStats();
    },
    caret(cursor) {
      sync.sendPresence({ ...me, cursor });
    },
  });

  const sync: SyncClient = new SyncClient(
    doc,
    `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/ws`,
    docName,
    {
      remote(patches) {
        editor.applyRemote(patches);
        persist();
        refreshStats();
      },
      status: (s) => showStatus(s),
      presence(p) {
        if (p.replica !== doc.replica) {
          editor.setPeer(p.replica, p);
          renderPeers();
        }
      },
      leave(replica) {
        editor.removePeer(replica);
        renderPeers();
      },
      error: (m) => console.warn("server:", m),
    },
  );

  // Other tabs of this browser share edits directly, even while offline.
  if (tabs) tabs.onmessage = (e) => sync.applyRemote((e.data as JsonOp[]).map(opFromJson));

  nameInput.addEventListener("change", () => {
    me.name = nameInput.value.trim().slice(0, 40) || me.name;
    try {
      localStorage.setItem("collab-name", me.name);
    } catch {}
    sync.sendPresence({ ...me, cursor: null });
  });

  const offlineBtn = $<HTMLButtonElement>("offline");
  offlineBtn.addEventListener("click", () => {
    const goOffline = offlineBtn.getAttribute("aria-pressed") !== "true";
    offlineBtn.setAttribute("aria-pressed", String(goOffline));
    offlineBtn.textContent = goOffline ? "Reconnect" : "Go offline";
    sync.setOffline(goOffline);
  });

  $("share").addEventListener("click", async () => {
    const url = `${location.origin}${location.pathname}?doc=${docName}`;
    try {
      await navigator.clipboard.writeText(url);
      $("share").textContent = "Link copied";
      setTimeout(() => ($("share").textContent = "Copy link"), 1500);
    } catch {
      prompt("Share this link", url);
    }
  });

  function showStatus(s: Status) {
    const el = $("status");
    el.dataset.state = s;
    el.textContent = { online: "Live", connecting: "Connecting…", offline: "Offline" }[s];
  }

  function renderPeers() {
    const list = $("peers");
    list.replaceChildren(
      ...[...editor.peers.values()].map((p) => {
        const li = document.createElement("li");
        li.style.setProperty("--c", p.color);
        li.textContent = p.name;
        return li;
      }),
    );
  }

  function refreshStats() {
    $("stats").textContent =
      `${doc.length} chars · ${doc.rawLength - doc.length} tombstones · ` +
      `${doc.vv.size} replicas seen · ${doc.log.length} ops`;
  }

  showStatus("offline");
  refreshStats();
  sync.connect();
  sync.sendPresence({ ...me, cursor: null });
  window.addEventListener("pagehide", () => saveOps(docName, doc.log));
  window.addEventListener("online", () => sync.retryNow());

  // Service worker: lets the app open with no network (needs HTTPS or localhost).
  if ("serviceWorker" in navigator && window.isSecureContext) {
    navigator.serviceWorker.register("sw.js").catch((e) => console.warn("service worker:", e));
  }
}

main();
