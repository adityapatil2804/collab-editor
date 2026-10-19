// Real-browser test (Chromium via Playwright) against a running server.
//
//   SERVER=127.0.0.1:8080 node test/browser.mjs [screenshot-dir]
//
// Two independent browser contexts (= two devices) edit the same document:
// live sync, remote cursors, offline editing + merge, own-edit undo,
// reload while offline (service worker + IndexedDB), and the UI layout.

import assert from "node:assert/strict";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
let chromium;
try {
  ({ chromium } = require("playwright"));
} catch {
  ({ chromium } = require("/opt/node22/lib/node_modules/playwright"));
}

const SERVER = process.env.SERVER ?? "127.0.0.1:8080";
const SHOTS = process.argv[2];
const DOC = `browser-${Date.now()}`;
const URL = `http://${SERVER}/?doc=${DOC}`;

const browser = await chromium.launch();
const errors = [];

async function device(name) {
  const ctx = await browser.newContext({ viewport: { width: 900, height: 640 } });
  const page = await ctx.newPage();
  page.on("pageerror", (e) => errors.push(`${name}: ${e.message}`));
  page.on("console", (m) => {
    // Failing to connect while the network is off is expected.
    if (m.type() === "error" && !m.text().includes("ERR_INTERNET_DISCONNECTED")) {
      errors.push(`${name} console: ${m.text()}`);
    }
  });
  await page.goto(URL);
  await page.fill("#name", name);
  await page.press("#name", "Enter");
  await page.waitForSelector('#status[data-state="online"]');
  return { ctx, page, name };
}

const text = (d) => d.page.inputValue("#editor");
async function until(what, fn, ms = 8000) {
  const end = Date.now() + ms;
  for (;;) {
    if (await fn()) return;
    if (Date.now() > end) throw new Error(`timed out: ${what}`);
    await new Promise((r) => setTimeout(r, 50));
  }
}
async function typeAt(d, pos, s) {
  await d.page.focus("#editor");
  await d.page.evaluate((p) => {
    const ta = document.getElementById("editor");
    ta.setSelectionRange(p, p);
  }, pos);
  await d.page.keyboard.type(s, { delay: 15 });
}

try {
  const A = await device("Alice");
  const B = await device("Bob");

  // 1. Live sync both ways.
  await typeAt(A, 0, "Hello from Alice. ");
  await until("B sees Alice", async () => (await text(B)) === "Hello from Alice. ");
  await typeAt(B, 18, "Hi Alice, Bob here 😀");
  await until("A sees Bob", async () => (await text(A)) === "Hello from Alice. Hi Alice, Bob here 😀");
  console.log("ok  live sync between two devices (incl. emoji)");

  // 2. Presence: each side lists the other and draws their cursor.
  await until("peers listed", async () =>
    (await A.page.textContent("#peers")).includes("Bob") &&
    (await B.page.textContent("#peers")).includes("Alice"));
  await typeAt(B, 5, "");
  await until("A draws Bob's cursor", async () => (await A.page.locator(".overlay .rc").count()) === 1);
  console.log("ok  presence list and remote cursor rendered");
  if (SHOTS) await A.page.screenshot({ path: `${SHOTS}/1-live.png` });

  // 3. Both go offline (real network loss), both edit, both come back.
  await A.ctx.setOffline(true);
  await B.ctx.setOffline(true);
  await A.page.click("#offline"); // also exercise the button path
  await until("A offline", async () => (await A.page.getAttribute("#status", "data-state")) === "offline");
  await typeAt(A, 0, "[A offline] ");
  const bLen = [...(await text(B))].length;
  await typeAt(B, (await text(B)).length, " [B offline]");
  assert.notEqual(await text(A), await text(B));
  if (SHOTS) await A.page.screenshot({ path: `${SHOTS}/2-offline.png` });

  // 4. Reload while offline: the page must still open with the offline edits.
  await A.page.reload();
  await A.page.waitForSelector("#editor");
  await until("A restored offline edits after reload", async () => (await text(A)).startsWith("[A offline] "));
  console.log("ok  page reloads with no network and keeps offline edits");

  await A.ctx.setOffline(false);
  await B.ctx.setOffline(false);
  await until("both back online", async () =>
    (await A.page.getAttribute("#status", "data-state")) === "online" &&
    (await B.page.getAttribute("#status", "data-state")) === "online", 20000);
  await until("merged", async () => {
    const [a, b] = [await text(A), await text(B)];
    return a === b && a.includes("[A offline]") && a.includes("[B offline]");
  });
  console.log("ok  offline edits from both devices merged:", JSON.stringify(await text(A)));
  void bLen;

  // 5. Undo only touches your own edits. (Undo history is per page load,
  //    so Alice makes a fresh edit after her reload.)
  const before = await text(A);
  await typeAt(A, 0, "UNDO ME ");
  await until("B sees new edit", async () => (await text(B)).startsWith("UNDO ME "));
  await typeAt(B, (await text(B)).length, " (Bob keeps this)");
  await until("A sees Bob", async () => (await text(A)).endsWith("(Bob keeps this)"));
  await A.page.focus("#editor");
  await A.page.keyboard.press("Control+z");
  await until("undo propagated", async () => {
    const [a, b] = [await text(A), await text(B)];
    return a === b && a === before + " (Bob keeps this)";
  });
  console.log("ok  Ctrl+Z removed only Alice's edit, on both devices");

  // 6. Layout sanity: overlay lines up with the textarea; nothing overflows.
  const box = await A.page.evaluate(() => {
    const ta = document.getElementById("editor").getBoundingClientRect();
    const ov = document.getElementById("overlay").getBoundingClientRect();
    return { dx: Math.abs(ta.x - ov.x), dy: Math.abs(ta.y - ov.y), scroll: document.documentElement.scrollWidth > innerWidth };
  });
  assert.ok(box.dx < 1 && box.dy < 1, "overlay misaligned");
  assert.ok(!box.scroll, "horizontal page scroll");
  if (SHOTS) {
    await A.page.screenshot({ path: `${SHOTS}/3-merged.png` });
    await A.page.setViewportSize({ width: 390, height: 760 });
    await A.page.screenshot({ path: `${SHOTS}/4-phone.png` });
  }
  console.log("ok  layout: overlay aligned, no horizontal scroll");

  assert.deepEqual(errors, [], "browser errors");
  console.log("ok  no page errors");
  await browser.close();
  process.exit(0);
} catch (e) {
  console.error("FAIL", e.message, errors);
  await browser.close();
  process.exit(1);
}
