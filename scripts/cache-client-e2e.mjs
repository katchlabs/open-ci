// Drives the compiled `open-ci cache` binary with the official @actions/cache toolkit client.
//
// Usage: node scripts/cache-client-e2e.mjs <open-ci-binary> <@actions/cache module dir> <work dir>
// The work dir must be empty and disposable. A loopback issuer signs RS256 runtime tokens with
// fixtures/actions-runtime-token-TEST-ONLY.pem exactly as GitHub shapes them.

import { createHash, createPrivateKey, createPublicKey, randomBytes, sign } from "node:crypto";
import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { mkdirSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const [binary, toolkitDir, requestedWorkDir] = process.argv.slice(2).map((p) => resolve(p));
if (!binary || !toolkitDir || !requestedWorkDir) {
  throw new Error("usage: cache-client-e2e.mjs <open-ci binary> <@actions/cache dir> <work dir>");
}

const REPO = 424242;
const MAIN = "refs/heads/main";
const PR = "refs/pull/7/merge";
const repoRoot = resolve(import.meta.dirname, "..");
const privateKey = createPrivateKey(
  readFileSync(join(repoRoot, "fixtures/actions-runtime-token-TEST-ONLY.pem")),
);
const jwk = { ...createPublicKey(privateKey).export({ format: "jwk" }), kid: "e2e", alg: "RS256", use: "sig" };

const issuer = await listen(createServer((req, res) => {
  if (req.url === "/.well-known/jwks") {
    res.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify({ keys: [jwk] }));
  } else {
    res.writeHead(404).end();
  }
}));

const artifactCalls = [];
const upstream = await listen(createServer((req, res) => {
  let body = "";
  req.on("data", (chunk) => { body += chunk; });
  req.on("end", () => {
    artifactCalls.push({ path: req.url, auth: req.headers.authorization, body });
    res.writeHead(200, { "content-type": "application/json" }).end('{"ok":true}');
  });
}));

const b64url = (data) => Buffer.from(data).toString("base64url");
function runtimeToken(repositoryId, grants) {
  const now = Math.floor(Date.now() / 1000);
  const header = b64url(JSON.stringify({ alg: "RS256", typ: "JWT", kid: "e2e" }));
  const payload = b64url(JSON.stringify({
    iss: issuer.url, nbf: now - 5, exp: now + 600,
    repository_id: String(repositoryId),
    ac: JSON.stringify(grants.map(([Scope, Permission]) => ({ Scope, Permission }))),
  }));
  return `${header}.${payload}.${b64url(sign("sha256", Buffer.from(`${header}.${payload}`), privateKey))}`;
}

const cachePort = await freePort();
const cacheUrl = `http://127.0.0.1:${cachePort}/`;
mkdirSync(requestedWorkDir, { recursive: true });
const workDir = realpathSync(requestedWorkDir);
writeFileSync(join(workDir, "signing.key"), randomBytes(48).toString("hex"));
const baseConfig = readFileSync(join(repoRoot, "config/open-ci.example.toml"), "utf8");
writeFileSync(join(workDir, "open-ci.toml"), `${baseConfig}
[cache]
listen = "127.0.0.1:${cachePort}"
public_url = "${cacheUrl}"
index_path = "${join(workDir, "index.db")}"
upstream_results_url = "${upstream.url}"

[cache.auth]
issuer = "${issuer.url}"
repositories = [${REPO}]

[cache.storage]
${process.env.OPEN_CI_E2E_STORAGE_TOML ?? `type = "local"\npath = "${join(workDir, "blobs")}"`}
`);

const server = spawn(binary, ["cache", "--config", join(workDir, "open-ci.toml")], {
  env: { ...process.env, OPEN_CI_CACHE_SIGNING_KEY_FILE: join(workDir, "signing.key"), RUST_LOG: process.env.RUST_LOG ?? "warn" },
  stdio: ["ignore", "inherit", "inherit"],
});
const failures = [];
try {
  await waitForHealth(`${cacheUrl}healthz`);
  process.env.ACTIONS_CACHE_SERVICE_V2 = "true";
  process.env.ACTIONS_RESULTS_URL = cacheUrl;
  process.env.GITHUB_SERVER_URL = "https://github.com";
  process.env.RUNNER_TEMP = join(workDir, "runner-temp");
  process.env.GITHUB_WORKSPACE = workDir;
  mkdirSync(process.env.RUNNER_TEMP, { recursive: true });
  process.chdir(workDir);
  const cache = await import(pathToFileURL(join(toolkitDir, "lib/cache.js")).href);

  const as = (token) => { process.env.ACTIONS_RUNTIME_TOKEN = token; };
  const mainToken = runtimeToken(REPO, [[MAIN, 3]]);
  const prToken = runtimeToken(REPO, [[MAIN, 1], [PR, 3]]);

  const small = writeTree("small", 3 * 1024 * 1024);
  const large = writeTree("large", 160 * 1024 * 1024);

  as(mainToken);
  check("main saves a single-shot archive", (await cache.saveCache(["small"], "small-v1")) > 0);
  check("main saves a block-staged archive (>128 MiB)", (await cache.saveCache(["large"], "large-v1")) > 0);

  for (const [dir, key, digest] of [["small", "small-v1", small], ["large", "large-v1", large]]) {
    rmSync(dir, { recursive: true, force: true });
    check(`main restores ${key} by exact key`, (await cache.restoreCache([dir], key)) === key);
    check(`${key} restores byte-identical`, digestTree(dir) === digest);
  }

  rmSync("small", { recursive: true, force: true });
  check("restore keys resolve by prefix", (await cache.restoreCache(["small"], "small-v2", ["small-"])) === "small-v1");

  as(prToken);
  rmSync("small", { recursive: true, force: true });
  check("pull request falls back to main", (await cache.restoreCache(["small"], "small-v1")) === "small-v1");
  const poisoned = writeTree("small", 1024 * 1024, "pr");
  check("pull request saves into its own ref", (await cache.saveCache(["small"], "small-v1")) > 0);

  as(mainToken);
  rmSync("small", { recursive: true, force: true });
  await cache.restoreCache(["small"], "small-v1");
  check("main never restores the pull request's entry", digestTree("small") === small && small !== poisoned);
  check("an existing entry is not overwritten", (await cache.saveCache(["small"], "small-v1")) === -1);

  as(runtimeToken(REPO + 1, [[MAIN, 3]]));
  rmSync("small", { recursive: true, force: true });
  check("another repository's token gets a miss", (await cache.restoreCache(["small"], "small-v1")) === undefined);

  as(mainToken);
  const artifact = await fetch(`${cacheUrl}twirp/github.actions.results.api.v1.ArtifactService/ListArtifacts`, {
    method: "POST", headers: { authorization: `Bearer ${mainToken}`, "content-type": "application/json" }, body: "{}",
  });
  check("artifact Twirp calls pass through to the results service",
    artifact.status === 200 && artifactCalls.length === 1 && artifactCalls[0].auth === `Bearer ${mainToken}`);
} catch (err) {
  failures.push(`unexpected error: ${err?.stack ?? err}`);
} finally {
  server.kill("SIGTERM");
  issuer.server.close();
  upstream.server.close();
}

if (failures.length > 0) {
  console.error(`\nFAIL: ${failures.length} @actions/cache e2e check(s) failed:\n  ${failures.join("\n  ")}`);
  process.exit(1);
}
console.log("\nok: official @actions/cache client passed every open-ci cache e2e check");

function check(name, passed) {
  console.log(`  ${passed ? "ok" : "FAIL"}: ${name}`);
  if (!passed) failures.push(name);
}

function writeTree(dir, bytes, seed = dir) {
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(join(dir, "nested"), { recursive: true });
  writeFileSync(join(dir, "nested", "payload.bin"), randomBytes(bytes));
  writeFileSync(join(dir, "manifest.txt"), `${seed}\n`);
  return digestTree(dir);
}

function digestTree(dir) {
  const hash = createHash("sha256");
  for (const file of ["manifest.txt", join("nested", "payload.bin")]) {
    try {
      hash.update(file).update(readFileSync(join(dir, file)));
    } catch {
      hash.update(`${file}:missing`);
    }
  }
  return hash.digest("hex");
}

function listen(server) {
  return new Promise((ok) => server.listen(0, "127.0.0.1", () => {
    ok({ server, url: `http://127.0.0.1:${server.address().port}` });
  }));
}

async function freePort() {
  const { server, url } = await listen(createServer());
  server.close();
  return Number(new URL(url).port);
}

async function waitForHealth(url) {
  for (let attempt = 0; attempt < 100; attempt += 1) {
    try {
      if ((await fetch(url)).ok) return;
    } catch {}
    await new Promise((ok) => setTimeout(ok, 100));
  }
  throw new Error("open-ci cache did not become healthy");
}
