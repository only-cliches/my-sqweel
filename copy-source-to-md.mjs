#!/usr/bin/env node

import { readdir, readFile, stat, writeFile } from "node:fs/promises";
import path from "node:path";
import process from "node:process";

const outputFile = process.argv[2] ?? "SOURCE_CODE.md";
const root = process.cwd();
const outputPath = path.resolve(root, outputFile);

const excludedDirs = new Set([
  ".git",
  "node_modules",
  "target",
  "dist",
  "build",
  "coverage",
]);

const includedExtensions = new Set([
  ".cjs",
  ".js",
  ".json",
  ".jsx",
  ".mjs",
  ".rs",
  ".ts",
  ".tsx",
]);

const includedFiles = new Set([
  "Cargo.toml",
  "package.json",
  "copy-source-to-md.mjs",
]);

const excludedFiles = new Set([
  "Cargo.lock",
  "package-lock.json",
  path.basename(outputPath),
]);

const languageByExtension = new Map([
  [".cjs", "js"],
  [".js", "js"],
  [".json", "json"],
  [".jsx", "jsx"],
  [".mjs", "js"],
  [".rs", "rust"],
  [".toml", "toml"],
  [".ts", "ts"],
  [".tsx", "tsx"],
]);

const sourceFiles = [
  "Cargo.toml",
  ...await collectSourceFiles(root + path.sep + "src")
].filter(v => v.indexOf("/vendor") == -1);

const markdown = await renderMarkdown(sourceFiles);
await writeFile(outputPath, markdown);

console.log(`Wrote ${sourceFiles.length} files to ${path.relative(root, outputPath)}`);

async function collectSourceFiles(dir) {
  const entries = await readdir(dir, { withFileTypes: true });
  const files = [];

  for (const entry of entries) {
    const absolutePath = path.join(dir, entry.name);
    const relativePath = path.relative(root, absolutePath);

    if (entry.isDirectory()) {
      if (excludedDirs.has(entry.name)) {
        continue;
      }
      files.push(...(await collectSourceFiles(absolutePath)));
      continue;
    }

    if (!entry.isFile() || !shouldIncludeFile(relativePath)) {
      continue;
    }

    const fileStat = await stat(absolutePath);
    if (fileStat.size === 0) {
      continue;
    }

    files.push(relativePath);
  }

  return files.sort((a, b) => a.localeCompare(b));
}

function shouldIncludeFile(relativePath) {
  const fileName = path.basename(relativePath);
  if (excludedFiles.has(fileName)) {
    return false;
  }
  if (includedFiles.has(relativePath) || includedFiles.has(fileName)) {
    return true;
  }
  return includedExtensions.has(path.extname(relativePath));
}

async function renderMarkdown(files) {
  const generatedAt = new Date().toISOString();
  const sections = [
    "# Source Code Export",
    "",
    `Generated at ${generatedAt}.`,
    "",
    "## Files",
    "",
    ...files.map((file) => `- [${file}](#${anchorFor(file)})`),
    "",
  ];

  for (const file of files) {
    const absolutePath = path.join(root, file);
    const contents = await readFile(absolutePath, "utf8");
    const fence = codeFenceFor(contents);
    const language = languageByExtension.get(path.extname(file)) ?? "";

    sections.push(
      `## ${file}`,
      "",
      `${fence}${language}`,
      contents.trimEnd(),
      fence,
      "",
    );
  }

  return `${sections.join("\n")}\n`;
}

function codeFenceFor(contents) {
  const longestFence = [...contents.matchAll(/`+/g)].reduce(
    (max, match) => Math.max(max, match[0].length),
    0,
  );
  return "`".repeat(Math.max(3, longestFence + 1));
}

function anchorFor(file) {
  return file
    .toLowerCase()
    .replace(/[^a-z0-9 _-]/g, "")
    .trim()
    .replace(/\s+/g, "-")
    .replace(/-+/g, "-");
}
