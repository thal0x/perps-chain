// End-to-end check of the trading UI against a running hlbft localnet.
//
//   HLBFT_API=http://127.0.0.1:3001 npm run e2e
//
// Env:
//   HLBFT_API         node API base URL (default http://127.0.0.1:3001)
//   HLBFT_ACCOUNTS    accounts.json for the dev server's key picker
//                     (default ../run/localnet/accounts.json)
//   HLBFT_KEY         use this private key instead of picking sdk_example
//   E2E_UI_URL        test an already-running UI instead of starting `vite`
//   E2E_SCREENSHOTS=1 also write docs/*.png (deviceScaleFactor 2)
//   E2E_HEADED=1      show the browser
import { spawn } from "node:child_process";
import { mkdirSync } from "node:fs";
import { createServer } from "node:net";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..");
const API = (process.env.HLBFT_API ?? "http://127.0.0.1:3001").replace(/\/+$/, "");
const SHOTS = process.env.E2E_SCREENSHOTS === "1";
const T = 20_000;

let step = 0;
const log = (m) => console.log(`[e2e ${String(++step).padStart(2, "0")}] ${m}`);

async function freePort() {
  return new Promise((res, rej) => {
    const s = createServer();
    s.listen(0, "127.0.0.1", () => {
      const p = s.address().port;
      s.close(() => res(p));
    });
    s.on("error", rej);
  });
}

async function startVite() {
  const port = await freePort();
  const env = { ...process.env, PORT: String(port) };
  if (process.env.HLBFT_ACCOUNTS) env.LOCALNET_ACCOUNTS = resolve(process.env.HLBFT_ACCOUNTS);
  const vite = resolve(root, "node_modules/vite/bin/vite.js");
  const child = spawn(process.execPath, [vite, "--port", String(port), "--strictPort", "--host", "127.0.0.1"], {
    cwd: root,
    env,
    stdio: ["ignore", "pipe", "pipe"],
  });
  let out = "";
  child.stdout.on("data", (d) => (out += d));
  child.stderr.on("data", (d) => (out += d));
  const url = `http://127.0.0.1:${port}`;
  for (let i = 0; i < 100; i++) {
    try {
      const r = await fetch(url);
      if (r.ok) return { url, stop: () => child.kill("SIGTERM") };
    } catch {
      /* not yet */
    }
    if (child.exitCode !== null) break;
    await new Promise((r) => setTimeout(r, 200));
  }
  child.kill("SIGKILL");
  throw new Error(`vite did not start:\n${out}`);
}

async function info(body) {
  const r = await fetch(`${API}/info`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(body) });
  if (!r.ok) throw new Error(`/info ${body.type}: HTTP ${r.status} ${await r.text()}`);
  return r.json();
}

async function main() {
  log(`API ${API}`);
  const mids = await info({ type: "allMids" }).catch((e) => {
    throw new Error(`localnet not reachable at ${API} (${e.message}). Start it with scripts/localnet.sh dev`);
  });
  if (!mids.BTC) throw new Error("no BTC mid; is the localnet seeded?");

  const ui = process.env.E2E_UI_URL ? { url: process.env.E2E_UI_URL.replace(/\/+$/, ""), stop: () => {} } : await startVite();
  log(`UI ${ui.url}`);
  const browser = await chromium.launch({ headless: process.env.E2E_HEADED !== "1" });
  const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: SHOTS ? 2 : 1 });
  const page = await ctx.newPage();
  const consoleErrors = [];
  page.on("pageerror", (e) => consoleErrors.push(String(e)));
  const shot = async (name) => {
    if (!SHOTS) return;
    mkdirSync(resolve(root, "docs"), { recursive: true });
    await page.screenshot({ path: resolve(root, "docs", name) });
    log(`screenshot docs/${name}`);
  };

  try {
    await page.goto(`${ui.url}/?api=${encodeURIComponent(API)}`);
    await page.evaluate(() => localStorage.clear());
    await page.goto(`${ui.url}/?api=${encodeURIComponent(API)}`);

    // 1. Load the funded sdk_example key.
    await page.getByTestId("open-settings").click();
    if (process.env.HLBFT_KEY) {
      await page.getByTestId("private-key").fill(process.env.HLBFT_KEY);
    } else {
      const sel = page.getByTestId("account-select");
      await page.waitForFunction(() => document.querySelectorAll('[data-testid="account-select"] option').length > 1, null, { timeout: T });
      const opt = await sel.locator("option", { hasText: "sdk_example" }).getAttribute("value");
      if (!opt) throw new Error("sdk_example not in accounts.json");
      await sel.selectOption(opt);
    }
    await page.getByTestId("settings-apply").click();
    const addr = await page.getByTestId("user-address").textContent({ timeout: T });
    log(`loaded key, address ${addr}`);

    // 2. BTC book populated.
    await page.getByTestId("market-picker").click();
    await page.getByTestId("market-BTC").click();
    await page.locator('[data-testid="book-bids"] .book-row').first().waitFor({ timeout: T });
    await page.locator('[data-testid="book-asks"] .book-row').first().waitFor({ timeout: T });
    const nb = await page.locator('[data-testid="book-bids"] .book-row').count();
    const na = await page.locator('[data-testid="book-asks"] .book-row').count();
    log(`BTC book: ${nb} bid / ${na} ask levels`);
    await page.waitForFunction(() => document.querySelector('[data-testid="ws-state"]')?.classList.contains("conn-open"), null, { timeout: T });
    log("WebSocket connected");
    const l2 = await info({ type: "l2Book", coin: "BTC" });
    const bestBid = Number(l2.levels[0][0].px);
    const bestAsk = Number(l2.levels[1][0].px);

    // Clean slate: no leftover BTC position / orders from earlier runs.
    await page.getByTestId("tab-positions").click();
    if (await page.getByTestId("close-BTC").count()) {
      await page.getByTestId("close-BTC").click();
      await page.getByTestId("position-BTC").waitFor({ state: "detached", timeout: T });
      log("closed leftover BTC position");
    }

    // 3. Rejected action shows the server's error string (ALO that would cross).
    await page.getByTestId("side-buy").click();
    await page.getByTestId("type-limit").click();
    await page.getByTestId("tif-Alo").check();
    await page.getByTestId("price").fill(String(Math.round(bestAsk * 1.01)));
    await page.getByTestId("size").fill("0.001");
    await page.getByTestId("submit-order").click();
    const err = page.getByTestId("toast-err").last();
    await err.waitFor({ timeout: T });
    log(`ALO crossing rejected, UI shows: ${(await err.textContent()).replace(/\s+/g, " ").trim()}`);
    await err.locator("button").click();

    // 4. Resting limit order appears in open orders.
    const restPx = String(Math.round(bestBid * 0.95));
    await page.getByTestId("tif-Gtc").check();
    await page.getByTestId("price").fill(restPx);
    await page.getByTestId("size").fill("0.001");
    await page.getByTestId("submit-order").click();
    const ok = page.getByTestId("toast-ok").filter({ hasText: "resting" }).last();
    await ok.waitFor({ timeout: T });
    const oid = (await ok.textContent()).match(/oid (\d+)/)?.[1];
    if (!oid) throw new Error(`no oid in toast: ${await ok.textContent()}`);
    log(`limit buy 0.001 @ ${restPx} resting, oid ${oid}`);
    await page.getByTestId("tab-orders").click();
    await page.getByTestId(`order-${oid}`).waitFor({ timeout: T });
    log(`order ${oid} visible in open orders`);
    await shot("open-order.png");

    // 5. Cancel it.
    await page.getByTestId(`cancel-${oid}`).click();
    await page.getByTestId(`order-${oid}`).waitFor({ state: "detached", timeout: T });
    log(`order ${oid} cancelled and gone from open orders`);

    // 6. Market buy fills and shows a position.
    await page.getByTestId("type-market").click();
    await page.getByTestId("size").fill("0.002");
    await page.getByTestId("submit-order").click();
    const filled = page.getByTestId("toast-ok").filter({ hasText: "filled" }).last();
    await filled.waitFor({ timeout: T });
    log(`market buy: ${(await filled.locator(".toast-body").textContent()).trim()}`);
    await page.getByTestId("tab-positions").click();
    const pos = page.getByTestId("position-BTC");
    await pos.waitFor({ timeout: T });
    log(`position row: ${(await pos.textContent()).replace(/\s+/g, " ").trim()}`);
    await page.waitForTimeout(1500); // let the chart/trades pick up the fill
    await shot("main.png");

    // 7. Close it.
    await page.getByTestId("close-BTC").click();
    await pos.waitFor({ state: "detached", timeout: T });
    log("position closed");

    if (SHOTS) {
      await page.getByTestId("tab-fills").click();
      await page.waitForTimeout(500);
      await shot("fills.png");
      await page.getByTestId("open-settings").click();
      await page.getByTestId("settings").waitFor();
      await shot("settings.png");
      await page.keyboard.press("Escape");
    }
    if (consoleErrors.length) throw new Error(`page errors:\n${consoleErrors.join("\n")}`);
    console.log("E2E PASS");
  } catch (e) {
    mkdirSync(resolve(here, "out"), { recursive: true });
    await page.screenshot({ path: resolve(here, "out", "failure.png") }).catch(() => {});
    console.error(`E2E FAIL: ${e.message}\n(screenshot: e2e/out/failure.png)`);
    process.exitCode = 1;
  } finally {
    await browser.close();
    ui.stop();
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
