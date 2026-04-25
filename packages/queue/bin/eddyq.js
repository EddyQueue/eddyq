#!/usr/bin/env node
"use strict";

const pkg = require("../package.json");

const USAGE = `eddyq ${pkg.version}

Usage:
  eddyq migrate run    [--database-url URL] [--line NAME]
  eddyq migrate list   [--database-url URL] [--line NAME]
  eddyq migrate down   [--database-url URL] [--line NAME] [--max-steps N] [--confirm]
  eddyq --help | --version

--database-url falls back to $DATABASE_URL.
--line defaults to "main".
migrate down without --confirm is a dry-run.
`;

function parseFlags(argv) {
  const flags = {};
  const positional = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--") {
      positional.push(...argv.slice(i + 1));
      break;
    }
    if (a.startsWith("--")) {
      const eq = a.indexOf("=");
      const key = eq === -1 ? a.slice(2) : a.slice(2, eq);
      let val;
      if (eq !== -1) {
        val = a.slice(eq + 1);
      } else if (i + 1 < argv.length && !argv[i + 1].startsWith("--")) {
        val = argv[++i];
      } else {
        val = true;
      }
      flags[key] = val;
    } else {
      positional.push(a);
    }
  }
  return { flags, positional };
}

function resolveDbUrl(flags) {
  const url = flags["database-url"] || process.env.DATABASE_URL;
  if (!url || url === true) {
    fail("missing --database-url (or $DATABASE_URL)");
  }
  return url;
}

function fail(msg, code = 1) {
  process.stderr.write(`error: ${msg}\n`);
  process.exit(code);
}

function fmtAppliedAt(iso) {
  if (!iso) return "— pending —";
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const pad = (n) => String(n).padStart(2, "0");
  return `${d.getUTCFullYear()}-${pad(d.getUTCMonth() + 1)}-${pad(d.getUTCDate())} ${pad(d.getUTCHours())}:${pad(d.getUTCMinutes())}:${pad(d.getUTCSeconds())} UTC`;
}

async function withQueue(flags, fn) {
  const url = resolveDbUrl(flags);
  const line = flags.line && flags.line !== true ? String(flags.line) : "main";
  const { Eddyq } = require("..");
  const q = await Eddyq.connect(url, { line });
  try {
    return await fn(q, line);
  } finally {
    try { await q.close(); } catch { /* ignore */ }
  }
}

async function cmdRun(flags) {
  await withQueue(flags, async (q, line) => {
    const report = await q.migrate();
    if (!report.applied.length) {
      console.log(`eddyq [line=${line}]: schema already up to date.`);
    } else {
      console.log(`eddyq [line=${line}]: applied ${report.applied.length} migration(s):`);
      for (const m of report.applied) console.log(`  + ${m.version}  ${m.name}`);
    }
  });
}

async function cmdList(flags) {
  await withQueue(flags, async (q, line) => {
    const statuses = await q.migrationStatus();
    console.log(`line: ${line}`);
    console.log(`${"#".padEnd(5)} ${"version".padEnd(16)} ${"name".padEnd(10)} applied_at`);
    console.log("-".repeat(60));
    statuses.forEach((s, i) => {
      const idx = String(i + 1).padEnd(5);
      const ver = String(s.version).padEnd(16);
      const name = String(s.name).padEnd(10);
      console.log(`${idx} ${ver} ${name} ${fmtAppliedAt(s.appliedAt)}`);
    });
  });
}

async function cmdDown(flags) {
  const maxStepsRaw = flags["max-steps"];
  const maxSteps = maxStepsRaw && maxStepsRaw !== true ? Number(maxStepsRaw) : 1;
  if (!Number.isInteger(maxSteps) || maxSteps < 1) {
    fail("--max-steps must be a positive integer", 2);
  }
  const confirm = flags.confirm === true;

  await withQueue(flags, async (q, line) => {
    if (!confirm) {
      const statuses = await q.migrationStatus();
      const toDrop = statuses.filter((s) => s.appliedAt).reverse().slice(0, maxSteps);
      process.stderr.write(
        `DRY RUN [line=${line}]. Would roll back ${toDrop.length} migration(s):\n`,
      );
      for (const s of toDrop) process.stderr.write(`  - ${s.version}  ${s.name}\n`);
      process.stderr.write("\nRe-run with --confirm to execute.\n");
      return;
    }
    const report = await q.migrateDown(maxSteps);
    if (!report.rolledBack.length) {
      console.log(`eddyq [line=${line}]: nothing to roll back.`);
    } else {
      console.log(`eddyq [line=${line}]: rolled back ${report.rolledBack.length} migration(s):`);
      for (const m of report.rolledBack) console.log(`  - ${m.version}  ${m.name}`);
    }
  });
}

async function main() {
  const argv = process.argv.slice(2);
  if (argv.length === 0 || argv[0] === "--help" || argv[0] === "-h") {
    process.stdout.write(USAGE);
    return;
  }
  if (argv[0] === "--version" || argv[0] === "-V") {
    console.log(pkg.version);
    return;
  }

  const { flags, positional } = parseFlags(argv);
  const [group, sub] = positional;

  if (group !== "migrate" || !sub) {
    process.stderr.write(USAGE);
    process.exit(2);
  }

  switch (sub) {
    case "run":  return cmdRun(flags);
    case "list": return cmdList(flags);
    case "down": return cmdDown(flags);
    default:
      fail(`unknown subcommand: migrate ${sub}`, 2);
  }
}

main().catch((e) => {
  fail(e && e.message ? e.message : String(e));
});
