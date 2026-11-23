/**
 * Pure, dependency-free OIDC admission client for `open-ci`.
 *
 * Security & reliability invariants:
 * - Never logs OIDC tokens, query parameters, or response bodies.
 * - Fails closed to caller-supplied fallback runner labels on any error.
 * - Never retries non-transient HTTP 4xx or schema-validation errors.
 */

export const ADMISSION_REASONS = [
  "admitted",
  "capacity",
  "unavailable",
  "rerun",
  "no_work",
  "deadline",
  "daily_limit",
  "lifetime_limit",
  "control_mode",
  "identity_unavailable",
  "admission_unavailable",
] as const;

export type AdmissionReason = (typeof ADMISSION_REASONS)[number];

export interface RouteOptions {
  endpoint: string;
  audience: string;
  pools: readonly string[];
  fallbacks: Readonly<Record<string, string>>;
  runAttempt: number;
  fallbackOnRerun?: boolean;
  requestUrl: string | undefined;
  requestToken: string | undefined;
  allowLoopback?: boolean;
  timeoutMs?: number;
  attemptMs?: number;
  fetchImpl?: typeof fetch;
  onDiagnostic?: (event: DiagnosticEvent) => void;
}

export interface DiagnosticEvent {
  phase: "identity_endpoint" | "identity_response" | "admission_request" | "admission_response";
  host?: string;
  status?: number;
  unavailable?: boolean;
}

export interface RouteDecision {
  admitted: boolean;
  target: "open-ci" | "fallback";
  reason: AdmissionReason;
  reservation: string;
  runners: Record<string, string>;
}

const SAFE_POOL_RE = /^[a-z0-9-]{1,24}$/;
const SAFE_LABEL_RE = /^[A-Za-z0-9_.-]{1,128}$/;
const RESERVATION_RE = /^open-ci-[a-f0-9]{32}$/;
const MAX_OIDC_RESPONSE_BYTES = 32 * 1024;
const MAX_ADMIT_RESPONSE_BYTES = 64 * 1024;

class NonRetryableError extends Error {}

function isSafePoolId(value: unknown): value is string {
  return typeof value === "string" && SAFE_POOL_RE.test(value);
}

function isSafeLabel(value: unknown): value is string {
  return typeof value === "string" && SAFE_LABEL_RE.test(value);
}

function cloneSafeMap(source: Readonly<Record<string, string>>): Record<string, string> {
  const out = Object.create(null) as Record<string, string>;
  for (const [k, v] of Object.entries(source)) {
    out[k] = v;
  }
  return out;
}

function parseValidatedUrl(raw: string, allowLoopback: boolean): URL {
  const url = new URL(raw);
  if (url.username || url.password || url.hash) {
    throw new NonRetryableError("URL must not contain credentials or fragment");
  }
  const isHttps = url.protocol === "https:";
  const isAllowedLoopback =
    allowLoopback &&
    url.protocol === "http:" &&
    (url.hostname === "127.0.0.1" || url.hostname === "localhost");
  if (!isHttps && !isAllowedLoopback) {
    throw new NonRetryableError("Endpoint must use HTTPS");
  }
  return url;
}

async function readBoundedJson(response: Response, maxBytes: number): Promise<unknown> {
  if (response.status >= 400 && response.status < 500) {
    await response.body?.cancel();
    throw new NonRetryableError(`HTTP ${response.status}`);
  }
  if (!response.ok) {
    await response.body?.cancel();
    throw new Error(`HTTP ${response.status}`);
  }
  const body = response.body;
  if (!body) {
    throw new Error("Empty response body");
  }
  const chunks: Uint8Array[] = [];
  let total = 0;
  for await (const chunk of body) {
    total += chunk.byteLength;
    if (total > maxBytes) {
      await body.cancel();
      throw new NonRetryableError("Response exceeds size limit");
    }
    chunks.push(chunk);
  }
  return JSON.parse(Buffer.concat(chunks).toString("utf8")) as unknown;
}

function buildFallback(
  fallbacks: Readonly<Record<string, string>>,
  reason: AdmissionReason,
): RouteDecision {
  return {
    admitted: false,
    target: "fallback",
    reason,
    reservation: "",
    runners: cloneSafeMap(fallbacks),
  };
}

function validateServerDecision(
  raw: unknown,
  pools: readonly string[],
  fallbacks: Readonly<Record<string, string>>,
): RouteDecision {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
    throw new NonRetryableError("Invalid decision payload");
  }
  const rec = raw as Record<string, unknown>;
  const admitted = Object.hasOwn(rec, "admitted") ? rec["admitted"] : undefined;
  const target = Object.hasOwn(rec, "target") ? rec["target"] : undefined;
  const reason = Object.hasOwn(rec, "reason") ? rec["reason"] : undefined;
  const reservation = Object.hasOwn(rec, "reservation") ? rec["reservation"] : undefined;
  const runners = Object.hasOwn(rec, "runners") ? rec["runners"] : undefined;

  if (
    typeof admitted !== "boolean" ||
    (target !== "open-ci" && target !== "fallback") ||
    typeof reason !== "string" ||
    !(ADMISSION_REASONS as readonly string[]).includes(reason) ||
    typeof reservation !== "string" ||
    typeof runners !== "object" ||
    runners === null ||
    Array.isArray(runners)
  ) {
    throw new NonRetryableError("Decision schema mismatch");
  }

  if (admitted && (target !== "open-ci" || reason !== "admitted")) {
    throw new NonRetryableError("Inconsistent admitted decision");
  }
  if (!admitted && (target !== "fallback" || reason === "admitted")) {
    throw new NonRetryableError("Inconsistent fallback decision");
  }

  const runnerMap = runners as Record<string, unknown>;
  if (Object.keys(runnerMap).length !== pools.length) {
    throw new NonRetryableError("Decision pool count mismatch");
  }

  const verifiedRunners = cloneSafeMap(fallbacks);
  for (const pool of pools) {
    if (!Object.hasOwn(runnerMap, pool)) {
      throw new NonRetryableError("Missing runner pool in server decision");
    }
    const label = runnerMap[pool];
    if (!isSafeLabel(label)) {
      throw new NonRetryableError("Unsafe runner label");
    }
    if (admitted) {
      if (!RESERVATION_RE.test(reservation)) {
        throw new NonRetryableError("Invalid reservation identifier");
      }
      if (!label.endsWith(`-${reservation}`)) {
        throw new NonRetryableError("Runner label not scoped to reservation");
      }
    } else if (label !== fallbacks[pool]) {
      throw new NonRetryableError("Fallback runner label mismatch");
    }
    verifiedRunners[pool] = label;
  }

  return {
    admitted,
    target,
    reason: reason as AdmissionReason,
    reservation: admitted ? reservation : "",
    runners: verifiedRunners,
  };
}

export async function route(options: RouteOptions): Promise<RouteDecision> {
  const {
    endpoint,
    audience,
    pools,
    fallbacks,
    runAttempt,
    fallbackOnRerun = true,
    requestUrl,
    requestToken,
    allowLoopback = false,
    timeoutMs = 15_000,
    attemptMs = 7_000,
    fetchImpl = fetch,
    onDiagnostic = () => {},
  } = options;

  if (
    pools.length > 32 ||
    !pools.every((pool) => isSafePoolId(pool)) ||
    new Set(pools).size !== pools.length
  ) {
    throw new Error("Invalid pools list");
  }
  const fallbackEntries = Object.entries(fallbacks);
  if (
    fallbackEntries.length > 32 ||
    !fallbackEntries.every(([pool, label]) => isSafePoolId(pool) && isSafeLabel(label)) ||
    !pools.every((pool) => Object.hasOwn(fallbacks, pool))
  ) {
    throw new Error("Invalid fallbacks configuration");
  }
  if (!Number.isSafeInteger(runAttempt) || runAttempt < 1) {
    throw new Error("Invalid runAttempt");
  }

  if (runAttempt > 1 && fallbackOnRerun) {
    return buildFallback(fallbacks, "rerun");
  }
  if (pools.length === 0) {
    return buildFallback(fallbacks, "no_work");
  }
  if (!requestUrl || !requestToken) {
    return buildFallback(fallbacks, "identity_unavailable");
  }

  const deadline = AbortSignal.timeout(timeoutMs);
  let phase: DiagnosticEvent["phase"] = "identity_endpoint";

  try {
    const oidcUrl = parseValidatedUrl(requestUrl, allowLoopback);
    onDiagnostic({ phase, host: oidcUrl.hostname });
    oidcUrl.searchParams.set("audience", audience);

    const oidcResponse = await fetchImpl(oidcUrl, {
      headers: { Authorization: `Bearer ${requestToken}` },
      redirect: "error",
      signal: deadline,
    });
    onDiagnostic({ phase: "identity_response", status: oidcResponse.status });

    const oidcBody = await readBoundedJson(oidcResponse, MAX_OIDC_RESPONSE_BYTES);
    if (typeof oidcBody !== "object" || oidcBody === null || Array.isArray(oidcBody)) {
      throw new NonRetryableError("Invalid OIDC response payload");
    }
    const oidcRec = oidcBody as Record<string, unknown>;
    const jwt = Object.hasOwn(oidcRec, "value") ? oidcRec["value"] : undefined;
    if (typeof jwt !== "string" || jwt.length === 0 || jwt.length > 16_384) {
      throw new NonRetryableError("Missing OIDC token value");
    }

    const admitUrl = parseValidatedUrl(endpoint, allowLoopback);
    if (admitUrl.search) {
      throw new NonRetryableError("Admission endpoint must not include query string");
    }

    const requestBody = JSON.stringify({ pools });

    for (let attempt = 0; attempt < 2; attempt++) {
      phase = "admission_request";
      try {
        const admitResponse = await fetchImpl(admitUrl, {
          method: "POST",
          headers: {
            Authorization: `Bearer ${jwt}`,
            "Content-Type": "application/json",
          },
          body: requestBody,
          redirect: "error",
          signal: AbortSignal.any([deadline, AbortSignal.timeout(attemptMs)]),
        });
        onDiagnostic({ phase: "admission_response", status: admitResponse.status });
        const raw = await readBoundedJson(admitResponse, MAX_ADMIT_RESPONSE_BYTES);
        return validateServerDecision(raw, pools, fallbacks);
      } catch (err) {
        onDiagnostic({ phase: "admission_response", unavailable: true });
        if (err instanceof NonRetryableError || deadline.aborted) {
          break;
        }
      }
    }
  } catch {
    onDiagnostic({ phase, unavailable: true });
  }

  return buildFallback(fallbacks, "admission_unavailable");
}
