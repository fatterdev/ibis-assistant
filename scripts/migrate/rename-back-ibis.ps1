#Requires -Version 7
<#
Revert of the F0.1 rebrand (decision 30/10/2026): threski -> ibis, product
"Ibis Assistant", short binary `ibis`, short MCP label "Ibis", data dir
%LOCALAPPDATA%\ibis, repo fatterdev/ibis-assistant. Runs IN PLACE on the
repo (git tracks the changes); historical documents are excluded and updated
by hand: CHANGELOG.md, docs/planning/PLAN.md, docs/planning/naming-map.md,
scripts/migrate/.

Usage:
  pwsh scripts/migrate/rename-back-ibis.ps1 -Roots Cargo.toml,Cargo.lock,crates
#>
param(
  [Parameter(Mandatory)] [string[]]$Roots
)

$ErrorActionPreference = "Stop"
$repo = (Resolve-Path (Join-Path $PSScriptRoot ".." "..")).Path

$excludeDirs = @(".git", "target", "node_modules", "dist", ".ruff_cache")
$excludeFileNames = @("CHANGELOG.md", "PLAN.md", "naming-map.md")
$excludeSubTrees = @("scripts/migrate")

# Inverse of docs/planning/naming-map.md — most specific first. Bare
# `threski` resolves to the SHORT new naming (binary `ibis`, label "Ibis",
# data dir `ibis`); only repo/VSIX identifiers keep `ibis-assistant`.
$rules = @(
  @{ Pattern = "Threski Studio";  Replacement = "Ibis Assistant"; Kind = "plain" },
  @{ Pattern = "THRESKI_";        Replacement = "IBIS_";          Kind = "plain" },
  @{ Pattern = "threski://";      Replacement = "ibis://";        Kind = "plain" },
  @{ Pattern = "threskiStudio";   Replacement = "ibis";           Kind = "plain" },
  @{ Pattern = "threski-studio";  Replacement = "ibis-assistant"; Kind = "plain" },
  @{ Pattern = "@threski/";       Replacement = "@ibis/";         Kind = "plain" },
  @{ Pattern = "__threski__";     Replacement = "__ibis__";       Kind = "plain" },
  @{ Pattern = "threski-(?=[a-z0-9])"; Replacement = "ibis-";     Kind = "regex" },
  @{ Pattern = "threski_(?=[a-z0-9])"; Replacement = "ibis_";     Kind = "regex" },
  @{ Pattern = "\bthreski\b";     Replacement = "ibis";           Kind = "regex" },
  @{ Pattern = "\bThreski\b";     Replacement = "Ibis";           Kind = "regex" },
  @{ Pattern = "\bTHRESKI\b";     Replacement = "IBIS";           Kind = "regex" }
)
$textExt = @(".rs", ".toml", ".lock", ".md", ".json", ".jsonc", ".js", ".mjs", ".cjs", ".ts", ".yml", ".yaml", ".ps1", ".sh", ".html", ".css", ".svg", ".mdc", ".example", ".txt", ".gitignore")
$specialNames = @("Dockerfile", ".gitattributes", ".gitignore", "NOTICE", "LICENSE", "sgconfig.yml")

$files = foreach ($r in $Roots) {
  $base = Join-Path $repo $r
  if (-not (Test-Path $base)) { throw "root does not exist: $base" }
  $item = Get-Item $base
  if ($item.PSIsContainer) {
    Get-ChildItem $item.FullName -Recurse -File
  } else {
    $item
  }
}
$files = $files | Where-Object {
  $rel = $_.FullName.Substring($repo.Length + 1).Replace("\", "/")
  -not ($excludeDirs | Where-Object { $rel -like "*$_/*" }) -and
  -not ($excludeSubTrees | Where-Object { $rel -like "$_*" }) -and
  -not ($excludeFileNames -contains $_.Name) -and
  ($textExt -contains $_.Extension -or $specialNames -contains $_.Name)
}

$counts = @{}
$touched = 0
foreach ($file in ($files | Sort-Object FullName -Unique)) {
  $content = Get-Content $file.FullName -Raw
  $original = $content
  foreach ($rule in $rules) {
    $before = $content
    if ($rule.Kind -eq "plain") { $content = $content.Replace($rule.Pattern, $rule.Replacement) }
    else { $content = [regex]::Replace($content, $rule.Pattern, $rule.Replacement) }
    if ($content -ne $before) { $counts[$rule.Pattern] = 1 + ($counts[$rule.Pattern] ?? 0) }
  }
  if ($content -ne $original) {
    Set-Content -LiteralPath $file.FullName -Value $content -NoNewline
    $touched++
    Write-Host ("renamed content: " + $file.FullName.Substring($repo.Length + 1))
  }
}

Write-Host "`nRevert summary: $touched file(s) touched"
$counts.GetEnumerator() | Sort-Object Name | ForEach-Object { Write-Host ("  {0,-28} applied" -f $_.Key) }