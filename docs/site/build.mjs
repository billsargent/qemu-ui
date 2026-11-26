// Builds the documentation site (X-04) from docs/*.md into docs/site/dist.
//
// Two pages are generated from data the code already checks:
// - platforms.md from data/parity.toml (the platform parity ledger)
// - api.md from data/openapi/openapi.json (kept current by the daemon's tests)
//
// Links between .md files become links between .html files. No deployment:
// CI builds the site to prove it builds; publish dist/ however you like.

import { mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { dirname, join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { Marked } from "marked";
import { parse as parseToml } from "smol-toml";

const here = dirname(fileURLToPath(import.meta.url));
const docs = join(here, "..");
const repo = join(docs, "..");
const out = join(here, "dist");

const esc = (s) =>
  String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);

const NAV = [
  ["index.md", "Overview"],
  ["install.md", "Install"],
  ["platforms.md", "Platform notes"],
  ["security.md", "Security model"],
  ["caveats.md", "Caveats"],
  ["cli.md", "Command-line client"],
  ["api.md", "HTTP API"],
  ["rules/README.md", "Validation rules"],
  ["process-survival.md", "Process survival"],
  ["design/privileged-helper.md", "Privileged helper"],
];

async function mdFiles(dir) {
  const found = [];
  for (const e of await readdir(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) {
      if (p === here) continue;
      found.push(...(await mdFiles(p)));
    } else if (e.name.endsWith(".md")) {
      found.push(p);
    }
  }
  return found;
}

const STATUS = {
  native: "Native",
  degraded: "Degraded",
  "needs-setup": "Needs setup",
  impossible: "Not possible",
  unverified: "Unverified",
};

function platformsPage(ledger) {
  const cell = (c) => `**${STATUS[c.status] ?? c.status}**${c.note ? ` ${c.note}` : ""}`;
  const rows = ledger.row
    .map((r) => `| ${r.feature} | ${cell(r.linux).replace(/\|/g, "\\|")} | ${cell(r.windows).replace(/\|/g, "\\|")} |`)
    .join("\n");
  return `# Platform notes

What each feature needs on each host platform. This page is generated from the
parity ledger (\`data/parity.toml\`); on a running daemon the System page shows
the same ledger refined by what the probe found on that host.

- **Native**: works out of the box.
- **Needs setup**: works after a one-time host change described in the note.
- **Degraded**: works with limits described in the note.
- **Not possible**: QEMU or the platform cannot do it.

| Feature | Linux | Windows |
|---|---|---|
${rows}
`;
}

function apiPage(doc) {
  const byTag = new Map();
  for (const [path, ops] of Object.entries(doc.paths)) {
    for (const [method, op] of Object.entries(ops)) {
      const tag = op.tags?.[0] ?? "Other";
      if (!byTag.has(tag)) byTag.set(tag, []);
      byTag.get(tag).push({ method, path, op });
    }
  }
  let s = `# HTTP API

${doc.info.description}

The machine-readable OpenAPI ${doc.openapi} document is served by the daemon at
\`GET /api/v1/openapi.json\` and is in the repository as
\`data/openapi/openapi.json\`; it is also published next to this page as
[openapi.json](openapi.json). Errors are JSON objects with an \`error\` string.
`;
  for (const [tag, ops] of byTag) {
    s += `\n## ${tag}\n\n| Method | Path | What |\n|---|---|---|\n`;
    for (const { method, path, op } of ops) {
      const body = op.requestBody
        ? ` Body: \`${op.requestBody.content["application/json"].schema.$ref.split("/").pop()}\`.`
        : "";
      s += `| \`${method.toUpperCase()}\` | \`${path}\` | ${op.summary}.${body} |\n`;
    }
  }
  return s;
}

function page(title, body, rel) {
  const up = rel.split(sep).length > 1 ? "../".repeat(rel.split(sep).length - 1) : "";
  const nav = NAV.map(
    ([f, t]) =>
      `<a href="${up}${f.replace(/\.md$/, ".html")}"${f === rel.split(sep).join("/") ? ' aria-current="page"' : ""}>${esc(t)}</a>`,
  ).join("\n");
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(title)} · qemu-ui</title>
<style>
:root { color-scheme: light dark; --fg: #1d1f23; --muted: #5b6270; --bg: #fff; --line: #e3e5ea; --accent: #2457c5; --code: #f4f5f7; }
@media (prefers-color-scheme: dark) { :root { --fg: #e6e8ec; --muted: #9aa1ad; --bg: #15171b; --line: #2a2e35; --accent: #7aa2ff; --code: #1f2228; } }
* { box-sizing: border-box; }
body { margin: 0; font: 16px/1.6 system-ui, sans-serif; color: var(--fg); background: var(--bg); }
.wrap { display: grid; grid-template-columns: 220px minmax(0, 1fr); max-width: 1100px; margin: 0 auto; }
nav { padding: 24px 16px; border-right: 1px solid var(--line); position: sticky; top: 0; align-self: start; }
nav strong { display: block; margin-bottom: 12px; }
nav a { display: block; padding: 4px 0; color: var(--muted); text-decoration: none; }
nav a[aria-current] { color: var(--fg); font-weight: 600; }
main { padding: 24px 32px 64px; min-width: 0; }
a { color: var(--accent); }
code { background: var(--code); padding: 1px 4px; border-radius: 4px; font-size: 0.9em; }
pre { background: var(--code); padding: 12px; border-radius: 6px; overflow-x: auto; }
pre code { padding: 0; background: none; }
table { border-collapse: collapse; display: block; overflow-x: auto; }
th, td { border: 1px solid var(--line); padding: 6px 10px; text-align: left; vertical-align: top; }
@media (max-width: 760px) { .wrap { grid-template-columns: 1fr; } nav { position: static; border-right: 0; border-bottom: 1px solid var(--line); } main { padding: 16px; } }
</style>
</head>
<body>
<div class="wrap">
<nav><strong>qemu-ui</strong>
${nav}
</nav>
<main>
${body}
</main>
</div>
</body>
</html>
`;
}

const marked = new Marked({
  gfm: true,
  walkTokens(t) {
    // foo.md and foo.md#x -> foo.html; leave absolute URLs alone.
    if (t.type === "link" && !/^[a-z]+:/i.test(t.href)) {
      t.href = t.href.replace(/\.md(#|$)/, ".html$1");
    }
  },
});

async function main() {
  await rm(out, { recursive: true, force: true });
  const pages = new Map();
  for (const f of await mdFiles(docs)) {
    pages.set(relative(docs, f), await readFile(f, "utf8"));
  }
  pages.set("platforms.md", platformsPage(parseToml(await readFile(join(repo, "data/parity.toml"), "utf8"))));
  const openapi = await readFile(join(repo, "data/openapi/openapi.json"), "utf8");
  pages.set("api.md", apiPage(JSON.parse(openapi)));

  for (const [rel, md] of pages) {
    const title = md.match(/^#\s+(.+)$/m)?.[1] ?? rel;
    const dest = join(out, rel.replace(/\.md$/, ".html"));
    await mkdir(dirname(dest), { recursive: true });
    await writeFile(dest, page(title, await marked.parse(md), rel));
  }
  await writeFile(join(out, "openapi.json"), openapi);

  // Every relative link must point at a page we wrote.
  const broken = [];
  for (const rel of pages.keys()) {
    const html = await readFile(join(out, rel.replace(/\.md$/, ".html")), "utf8");
    for (const [, href] of html.matchAll(/href="([^"#]+)(?:#[^"]*)?"/g)) {
      if (/^[a-z]+:/i.test(href)) continue;
      const target = join(dirname(join(out, rel)), href);
      try {
        await readFile(target);
      } catch {
        broken.push(`${rel} -> ${href}`);
      }
    }
  }
  if (broken.length) {
    console.error(`broken links:\n  ${broken.join("\n  ")}`);
    process.exit(1);
  }
  console.log(`built ${pages.size} pages into ${relative(repo, out)}`);
}

await main();
