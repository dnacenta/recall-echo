import { existsSync, mkdirSync, readFileSync, writeFileSync } from "fs";
import { join } from "path";
import { homedir } from "os";
import { createInterface } from "readline";

const CLAUDE_DIR = join(homedir(), ".claude");

const PATHS = {
  rulesDir: join(CLAUDE_DIR, "rules"),
  rulesFile: join(CLAUDE_DIR, "rules", "recall-echo.md"),
  memoryDir: join(CLAUDE_DIR, "memory"),
  memoryFile: join(CLAUDE_DIR, "memory", "MEMORY.md"),
  memoriesDir: join(CLAUDE_DIR, "memories"),
  ephemeralFile: join(CLAUDE_DIR, "EPHEMERAL.md"),
  archiveFile: join(CLAUDE_DIR, "ARCHIVE.md"),
  settingsFile: join(CLAUDE_DIR, "settings.json"),
};

const TEMPLATES = {
  memory: `# Memory

<!-- recall-echo: Curated memory. Distilled facts, preferences, patterns. -->
<!-- Keep under 200 lines. Only write confirmed, stable information. -->
`,
  ephemeral: "",
  archive: `# Archive Index

<!-- recall-echo: Lightweight index of archive logs. -->
<!-- Format: | log number | date | key topics | -->
`,
};

const PRECOMPACT_HOOK = {
  hooks: [
    {
      type: "command" as const,
      command:
        "echo 'RECALL-ECHO: Context compaction imminent. Save a memory checkpoint to ~/.claude/memories/ before context is lost. Check the highest archive-log-XXX.md number and create the next one.'",
    },
  ],
};

function getProtocolTemplate(): string {
  // Try to read from templates directory (npm package context)
  const templatePaths = [
    join(__dirname, "..", "templates", "recall-echo.md"),
    join(__dirname, "..", "..", "templates", "recall-echo.md"),
  ];

  for (const p of templatePaths) {
    if (existsSync(p)) {
      return readFileSync(p, "utf-8");
    }
  }

  // Fallback: return a minimal protocol
  return `# recall-echo — Memory Protocol

You have a persistent three-layer memory system. Consult the recall-echo documentation for full protocol details.

## Quick Reference
- Layer 1 (MEMORY.md): Curated facts at ~/.claude/memory/MEMORY.md — always in context
- Layer 2 (EPHEMERAL.md): Last session summary at ~/.claude/EPHEMERAL.md — read then clear at session start, rewrite at session end
- Layer 3 (Archive): Logs at ~/.claude/memories/archive-log-XXX.md — search with Grep on demand

@~/.claude/EPHEMERAL.md
`;
}

async function confirm(question: string): Promise<boolean> {
  const rl = createInterface({ input: process.stdin, output: process.stdout });
  return new Promise((resolve) => {
    rl.question(`${question} [y/N] `, (answer) => {
      rl.close();
      resolve(answer.toLowerCase() === "y" || answer.toLowerCase() === "yes");
    });
  });
}

function ensureDir(dir: string): boolean {
  if (!existsSync(dir)) {
    mkdirSync(dir, { recursive: true });
    return true;
  }
  return false;
}

function writeIfNotExists(filePath: string, content: string, label: string): string {
  if (existsSync(filePath)) {
    return `  exists: ${label} (${filePath})`;
  }
  writeFileSync(filePath, content, "utf-8");
  return `  created: ${label} (${filePath})`;
}

async function writeWithConfirm(
  filePath: string,
  content: string,
  label: string
): Promise<string> {
  if (existsSync(filePath)) {
    const existing = readFileSync(filePath, "utf-8");
    if (existing === content) {
      return `  exists: ${label} — already up to date`;
    }
    const shouldOverwrite = await confirm(
      `  ${label} already exists at ${filePath}. Overwrite?`
    );
    if (!shouldOverwrite) {
      return `  skipped: ${label} (kept existing)`;
    }
  }
  writeFileSync(filePath, content, "utf-8");
  return `  created: ${label} (${filePath})`;
}

function mergePreCompactHook(settingsPath: string): string {
  let settings: Record<string, unknown> = {};

  if (existsSync(settingsPath)) {
    try {
      settings = JSON.parse(readFileSync(settingsPath, "utf-8"));
    } catch {
      return `  error: Could not parse ${settingsPath}. Add PreCompact hook manually.`;
    }
  }

  const hooks = (settings.hooks ?? {}) as Record<string, unknown[]>;

  // Check if PreCompact hook already has recall-echo
  const existing = hooks.PreCompact as Array<Record<string, unknown>> | undefined;
  if (existing) {
    const hasRecallEcho = existing.some((h) => {
      const innerHooks = h.hooks as Array<Record<string, string>> | undefined;
      return innerHooks?.some((ih) => ih.command?.includes("RECALL-ECHO"));
    });
    if (hasRecallEcho) {
      return `  exists: PreCompact hook — already configured`;
    }
  }

  // Merge the hook
  hooks.PreCompact = [...(existing ?? []), PRECOMPACT_HOOK];
  settings.hooks = hooks;

  writeFileSync(settingsPath, JSON.stringify(settings, null, 2) + "\n", "utf-8");
  return `  created: PreCompact hook in settings.json`;
}

export async function init(): Promise<void> {
  console.log("\nrecall-echo — initializing memory system\n");

  // Check if ~/.claude exists
  if (!existsSync(CLAUDE_DIR)) {
    console.log(
      "  ~/.claude directory not found. Is Claude Code installed?\n" +
        "  Install Claude Code first, then run this again.\n"
    );
    process.exit(1);
  }

  const results: string[] = [];

  // 1. Create directories
  ensureDir(PATHS.rulesDir);
  ensureDir(PATHS.memoryDir);
  ensureDir(PATHS.memoriesDir);

  // 2. Write the memory protocol rules file (always update to latest)
  const protocol = getProtocolTemplate();
  results.push(await writeWithConfirm(PATHS.rulesFile, protocol, "Memory protocol (rules file)"));

  // 3. Write MEMORY.md (never overwrite — user's curated data)
  results.push(writeIfNotExists(PATHS.memoryFile, TEMPLATES.memory, "MEMORY.md"));

  // 4. Write EPHEMERAL.md (never overwrite — may have active session data)
  results.push(writeIfNotExists(PATHS.ephemeralFile, TEMPLATES.ephemeral, "EPHEMERAL.md"));

  // 5. Write ARCHIVE.md (never overwrite — contains history index)
  results.push(writeIfNotExists(PATHS.archiveFile, TEMPLATES.archive, "ARCHIVE.md"));

  // 6. Merge PreCompact hook into settings.json
  results.push(mergePreCompactHook(PATHS.settingsFile));

  // Print results
  console.log("Results:\n");
  results.forEach((r) => console.log(r));

  console.log(`
Setup complete. Your memory system is ready.

How it works:
  Layer 1 (MEMORY.md)     — Curated facts, always in context
  Layer 2 (EPHEMERAL.md)  — Last session summary, read then cleared
  Layer 3 (Archive)       — Searchable history in ~/.claude/memories/

The memory protocol is loaded automatically via ~/.claude/rules/recall-echo.md.
Start a new Claude Code session and your agent will have persistent memory.
`);
}

// __dirname equivalent for ES modules
import { dirname } from "path";
import { fileURLToPath } from "url";
const __dirname = dirname(fileURLToPath(import.meta.url));
