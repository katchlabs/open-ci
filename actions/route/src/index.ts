import { appendFileSync } from "node:fs";
import { route } from "./route.ts";

function requiredInput(name: string): string {
  const value = process.env[`INPUT_${name.toUpperCase()}`]?.trim() ?? "";
  if (!value) {
    throw new Error(`Missing required action input: ${name}`);
  }
  return value;
}

function writeGitHubOutputs(outputs: Record<string, string>): void {
  const outputPath = process.env["GITHUB_OUTPUT"];
  if (!outputPath) {
    throw new Error("GITHUB_OUTPUT environment variable is not set");
  }
  for (const [key, value] of Object.entries(outputs)) {
    if (value.includes("\n") || value.includes("\r")) {
      throw new Error(`Output ${key} contains forbidden newline`);
    }
    appendFileSync(outputPath, `${key}=${value}\n`, "utf8");
  }
}

function parseStringArray(raw: string, name: string): string[] {
  const parsed: unknown = JSON.parse(raw);
  if (!Array.isArray(parsed) || !parsed.every((v) => typeof v === "string")) {
    throw new Error(`Input ${name} must be a JSON array of strings`);
  }
  return parsed;
}

function parseStringRecord(raw: string, name: string): Record<string, string> {
  const parsed: unknown = JSON.parse(raw);
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new Error(`Input ${name} must be a JSON object mapping pool IDs to labels`);
  }
  const out = Object.create(null) as Record<string, string>;
  for (const [k, v] of Object.entries(parsed as Record<string, unknown>)) {
    if (typeof v !== "string") {
      throw new Error(`Input ${name} values must be strings`);
    }
    out[k] = v;
  }
  return out;
}

async function main(): Promise<void> {
  const pools = parseStringArray(requiredInput("pools"), "pools");
  const fallbacks = parseStringRecord(requiredInput("fallbacks"), "fallbacks");
  const fallbackOnRerunRaw = (process.env["INPUT_FALLBACK-ON-RERUN"] ?? "true").trim().toLowerCase();
  const fallbackOnRerun = fallbackOnRerunRaw !== "false";

  const decision = await route({
    endpoint: requiredInput("endpoint"),
    audience: requiredInput("audience"),
    pools,
    fallbacks,
    runAttempt: Number(process.env["GITHUB_RUN_ATTEMPT"] ?? "1"),
    fallbackOnRerun,
    requestUrl: process.env["ACTIONS_ID_TOKEN_REQUEST_URL"],
    requestToken: process.env["ACTIONS_ID_TOKEN_REQUEST_TOKEN"],
    onDiagnostic: (event) => {
      console.log(`open-ci diagnostic: ${JSON.stringify(event)}`);
    },
  });

  writeGitHubOutputs({
    admitted: String(decision.admitted),
    target: decision.target,
    reason: decision.reason,
    reservation: decision.reservation,
    runners: JSON.stringify(decision.runners),
  });
  console.log(`open-ci routing: target=${decision.target} reason=${decision.reason}`);
}

main().catch(() => {
  console.error("open-ci: invalid workflow input or GITHUB_OUTPUT unavailable");
  process.exitCode = 1;
});
