#Requires -Version 7
<#
F0.1 global rename: reads the source repo READ-ONLY and writes a migrated tree
ibis -> threski ("Threski Studio"). See docs/planning/PLAN.md section 5 and
docs/planning/naming-map.md.

Usage:
  pwsh scripts/migrate/migrate-rename.ps1 -Source C:\devsrc\ibis-assistant -Destination C:\devsrc\threski-studio\runtime [-Force]
#>
param(
  [Parameter(Mandatory)] [string]$Source,
  [Parameter(Mandatory)] [string]$Destination,
  [switch]$Force
)

$ErrorActionPreference = "Stop"
$Source = (Resolve-Path $Source).Path
if (-not (Test-Path (Join-Path $Source "Cargo.toml"))) {
  throw "Source does not look like the product repo (no Cargo.toml): $Source"
}
if ((Resolve-Path $Destination -ErrorAction SilentlyContinue) -and ((Resolve-Path $Destination).Path.StartsWith($Source))) {
  throw "Destination must not live inside Source"
}
if (Test-Path $Destination) {
  if (-not $Force) { throw "Destination exists, use -Force to recreate: $Destination" }
  Remove-Item -Recurse -Force $Destination
}

$excludeDirs = @(".git", "target", "node_modules", "dist", ".ruff_cache")
$excludeFiles = @("debug-*.log")
robocopy $Source $Destination /E /NFL /NDL /NJH /NJS @($excludeDirs | ForEach-Object { "/XD"; $_ }) @($excludeFiles | ForEach-Object { "/XF"; $_ }) | Out-Null
if ($LASTEXITCODE -ge 8) { throw "robocopy failed with code $LASTEXITCODE" }
Write-Host "Copied tree -> $Destination"

$crates = Join-Path $Destination "crates"
if (Test-Path $crates) {
  Get-ChildItem $crates -Directory -Filter "ibis-*" | ForEach-Object {
    $new = Join-Path $crates ($_.Name -replace "^ibis-", "threski-")
    Rename-Item $_.FullName $new
    Write-Host "renamed crate dir: $($_.Name) -> $(Split-Path $new -Leaf)"
  }
}

$rules = @(
  @{ Pattern = "ibis-assistant"; Replacement = "threski"; Kind = "plain" },
  @{ Pattern = "IBIS_"; Replacement = "THRESKI_"; Kind = "plain" },
  @{ Pattern = "ibis://"; Replacement = "threski://"; Kind = "plain" },
  @{ Pattern = "Ibis Assistant"; Replacement = "Threski Studio"; Kind = "plain" },
  @{ Pattern = "ibis-(?=[a-z0-9])"; Replacement = "threski-"; Kind = "regex" },
  @{ Pattern = "ibis_(?=[a-z0-9])"; Replacement = "threski_"; Kind = "regex" },
  @{ Pattern = "\bibis\.(?=[a-z])"; Replacement = "threskiStudio."; Kind = "regex" },
  @{ Pattern = "#ibis\."; Replacement = "#threskiStudio."; Kind = "regex" },
  @{ Pattern = "\bibis\b"; Replacement = "threski"; Kind = "regex" },
  @{ Pattern = "\bIbis\b"; Replacement = "Threski"; Kind = "regex" },
  @{ Pattern = "\bIBIS\b"; Replacement = "THRESKI"; Kind = "regex" }
)
$textExt = @(".rs", ".toml", ".lock", ".md", ".json", ".jsonc", ".js", ".mjs", ".cjs", ".ts", ".yml", ".yaml", ".ps1", ".sh", ".html", ".css", ".svg", ".mdc", ".example", ".txt", ".gitignore")
$counts = @{}
$files = Get-ChildItem $Destination -Recurse -File | Where-Object {
  $rel = $_.FullName.Substring($Destination.Length)
  -not ($excludeDirs | Where-Object { $rel -like "*\$_*" }) -and ($textExt -contains $_.Extension -or $_.Name -in @("Dockerfile", ".gitattributes", ".gitignore", "NOTICE", "LICENSE", "sgconfig.yml"))
}
foreach ($file in $files) {
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
  }
}
Write-Host "`nReplacement summary:"
$counts.GetEnumerator() | Sort-Object Name | ForEach-Object { Write-Host ("  {0,-28} applied" -f $_.Key) }

Write-Host "`nLeftover 'ibis' occurrences (review manually; lore references to the bird are intentional):"
$leftovers = Get-ChildItem $Destination -Recurse -File |
  Where-Object { $rel = $_.FullName.Substring($Destination.Length); -not ($excludeDirs | Where-Object { $rel -like "*\$_*" }) } |
  Select-String -Pattern "ibis" -SimpleMatch
$leftovers | Select-Object -First 200 | ForEach-Object { Write-Host ("  {0}:{1}: {2}" -f $_.Path.Substring($Destination.Length + 1), $_.LineNumber, $_.Line.Trim().Substring(0, [Math]::Min(120, $_.Line.Trim().Length))) }
Write-Host ("  total files with leftovers: " + (($leftovers | ForEach-Object { $_.Path } | Sort-Object -Unique).Count))

Write-Host "`nNext steps (see scripts/migrate/migrate-checklist.md):"
Write-Host "  1. cd $Destination; cargo test; cargo xtask dist"
Write-Host "  2. cd vscode-extension; npm run package"
Write-Host "  3. review leftovers above and docs lore"