import { dirname, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const tscBin = join(
  projectRoot,
  "node_modules",
  "typescript",
  "bin",
  "tsc",
);

const typecheck = spawnSync(
  process.execPath,
  [tscBin, "-p", "tsconfig.qtui.json"],
  {
    cwd: projectRoot,
    stdio: "inherit",
    shell: false,
  },
);

if (typecheck.error) {
  console.error(typecheck.error);
}

if (typecheck.status !== 0) {
  process.exit(typecheck.status ?? 1);
}

console.log("QTUI TypeScript sources compiled to omnicrawl/ui/qt/web");
