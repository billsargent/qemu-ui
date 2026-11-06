// Capture UI screenshots against a running daemon, using the installed
// Microsoft Edge (no browser download). Usage:
//   node e2e/screenshots.mjs <base-url> <out-dir> <route>=<file> [...]
// Example:
//   node e2e/screenshots.mjs http://127.0.0.1:7480 shots "#/dashboard=dashboard"
import { chromium } from "playwright-core";

const [base, out, ...routes] = process.argv.slice(2);
const browser = await chromium.launch({ channel: "msedge", headless: true });
const logs = [];
for (const r of routes) {
  const [route, file, wait = "2500"] = r.split("=");
  // A fresh page per route: a full load, not a same-document hash change.
  const page = await browser.newPage({ viewport: { width: 1600, height: 1000 } });
  page.on("console", (m) => m.type() === "error" && logs.push(`${file} console: ${m.text()}`));
  page.on("pageerror", (e) => logs.push(`${file} pageerror: ${e.message}`));
  await page.goto(`${base}/${route}`);
  await page.waitForTimeout(Number(wait));
  await page.screenshot({ path: `${out}/${file}.png`, fullPage: true });
  const h1 = await page.locator("h1").first().textContent().catch(() => "?");
  console.log(`saved ${file}.png  url=${page.url()}  h1=${h1}`);
  await page.close();
}
if (logs.length) console.log(logs.join("\n"));
await browser.close();
