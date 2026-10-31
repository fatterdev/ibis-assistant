#!/usr/bin/env node
/**
 * Keyboard driver for the R3/R4 UI tasks (Parte B). Zero dependencies.
 *
 * Activates a VS Code window and sends WScript.Shell SendKeys sequences (or
 * clipboard-pastes literal text), so computer-use tasks can be executed
 * without a GUI agent on deterministic palette/tree/chat flows.
 *
 * Usage:
 *   node scripts/ui-test/keys.mjs --find ibis-assistant --keys "^+p"
 *   node scripts/ui-test/keys.mjs --find ibis-assistant --text "UI smoke"
 *   node scripts/ui-test/keys.mjs --find ibis-assistant --keys "{ENTER}" --wait 800
 */

import { spawnSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const args = process.argv.slice(2);
const opt = (name) => {
  const i = args.indexOf(name);
  return i >= 0 && args[i + 1] !== undefined ? args[i + 1] : undefined;
};

const find = opt("--find");
const keys = opt("--keys");
const text = opt("--text");
const focusOnly = args.includes("--focus");
const waitMs = Number(opt("--wait") || 250);

if (!find || (!keys && !text && !focusOnly)) {
  console.error(
    "usage: keys.mjs --find <title substring> (--focus | --keys <SendKeys> | --text <literal>) [--process <name>] [--wait ms]",
  );
  process.exit(1);
}

const PS1 = `
$find = ${JSON.stringify(find)}
$procName = ${JSON.stringify(opt("--process") || "Code")}
$p = Get-Process -Name $procName -ErrorAction SilentlyContinue |
  Where-Object { $_.MainWindowHandle -ne 0 -and $_.MainWindowTitle -like "*$find*" } |
  Sort-Object -Property MainWindowTitle |
  Select-Object -First 1
if (-not $p) {
  Write-Error ("window not found: " + $find)
  exit 1
}
$sh = New-Object -ComObject WScript.Shell
$ok = $sh.AppActivate($p.Id)
if (-not $ok) { Start-Sleep -Milliseconds 400; $ok = $sh.AppActivate($p.Id) }
if (-not $ok) { Write-Error ("could not activate: " + $p.MainWindowTitle); exit 1 }
Start-Sleep -Milliseconds 350
${text ? `$t = ${JSON.stringify(text)}
Set-Clipboard -Value $t
$sh.SendKeys("^v")
Write-Output ("pasted to: " + $p.MainWindowTitle)` : focusOnly ? `Write-Output ("focused: " + $p.MainWindowTitle)` : `$sh.SendKeys(${JSON.stringify(keys)})
Write-Output ("sent keys to: " + $p.MainWindowTitle)`}
`;

const dir = mkdtempSync(join(tmpdir(), "ibis-keys-"));
const ps1 = join(dir, "keys.ps1");
writeFileSync(ps1, PS1, "utf8");
const res = spawnSync("powershell", ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", ps1], {
  timeout: 30_000,
  encoding: "utf8",
});
rmSync(dir, { recursive: true, force: true });

process.stdout.write(res.stdout || "");
if (res.status !== 0) {
  process.stderr.write(res.stderr || "keys.ps1 failed");
  process.exit(res.status || 1);
}

if (waitMs > 0) {
  spawnSync("powershell", ["-NoProfile", "-Command", `Start-Sleep -Milliseconds ${waitMs}`], {
    timeout: waitMs + 5_000,
  });
}