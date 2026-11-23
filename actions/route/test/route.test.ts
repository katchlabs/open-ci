import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { route } from "../src/route.ts";

const RESERVATION = "open-ci-0123456789abcdef0123456789abcdef";

describe("open-ci route client", () => {
  it("admits runs when OIDC and control plane return a valid scoped decision", async () => {
    const fetchMock: typeof fetch = async (input) => {
      const url = String(input);
      if (url.includes("oidc.example")) {
        return Response.json({ value: "signed-jwt-token" });
      }
      return Response.json({
        admitted: true,
        target: "open-ci",
        reason: "admitted",
        reservation: RESERVATION,
        runners: {
          api: `open-ci-api-${RESERVATION}`,
        },
      });
    };

    const decision = await route({
      endpoint: "https://ci.example.com/v1/ingress/admit",
      audience: "open-ci:acme/app",
      pools: ["api"],
      fallbacks: { api: "ubuntu-latest" },
      runAttempt: 1,
      requestUrl: "https://oidc.example/token",
      requestToken: "req-token",
      fetchImpl: fetchMock,
    });

    assert.equal(decision.admitted, true);
    assert.equal(decision.target, "open-ci");
    assert.equal(decision.reason, "admitted");
    assert.equal(decision.reservation, RESERVATION);
    assert.equal(decision.runners["api"], `open-ci-api-${RESERVATION}`);
  });

  it("does not retry non-transient 401/403 responses and fails closed to fallback", async () => {
    let admitCalls = 0;
    const fetchMock: typeof fetch = async (input) => {
      const url = String(input);
      if (url.includes("oidc.example")) {
        return Response.json({ value: "signed-jwt-token" });
      }
      admitCalls += 1;
      return new Response("unauthorized", { status: 401 });
    };

    const decision = await route({
      endpoint: "https://ci.example.com/v1/ingress/admit",
      audience: "open-ci:acme/app",
      pools: ["api"],
      fallbacks: { api: "ubuntu-latest" },
      runAttempt: 1,
      requestUrl: "https://oidc.example/token",
      requestToken: "req-token",
      fetchImpl: fetchMock,
    });

    assert.equal(admitCalls, 1);
    assert.equal(decision.admitted, false);
    assert.equal(decision.target, "fallback");
    assert.equal(decision.reason, "admission_unavailable");
    assert.equal(decision.runners["api"], "ubuntu-latest");
  });

  it("routes reruns directly to fallback without calling OIDC or admission", async () => {
    let called = false;
    const fetchMock: typeof fetch = async () => {
      called = true;
      return Response.json({});
    };

    const decision = await route({
      endpoint: "https://ci.example.com/v1/ingress/admit",
      audience: "open-ci:acme/app",
      pools: ["api"],
      fallbacks: { api: "ubuntu-latest" },
      runAttempt: 2,
      requestUrl: "https://oidc.example/token",
      requestToken: "req-token",
      fetchImpl: fetchMock,
    });

    assert.equal(called, false);
    assert.equal(decision.admitted, false);
    assert.equal(decision.reason, "rerun");
    assert.equal(decision.runners["api"], "ubuntu-latest");
  });

  it("allows reruns to request admission when fallbackOnRerun is false", async () => {
    const fetchMock: typeof fetch = async (input) => {
      const url = String(input);
      if (url.includes("oidc.example")) {
        return Response.json({ value: "signed-jwt-token" });
      }
      return Response.json({
        admitted: true,
        target: "open-ci",
        reason: "admitted",
        reservation: RESERVATION,
        runners: { default: `open-ci-linux-x64-${RESERVATION}` },
      });
    };

    const decision = await route({
      endpoint: "https://ci.example.com/v1/ingress/admit",
      audience: "open-ci:acme/app",
      pools: ["default"],
      fallbacks: { default: "ubuntu-latest" },
      runAttempt: 2,
      fallbackOnRerun: false,
      requestUrl: "https://oidc.example/token",
      requestToken: "req-token",
      fetchImpl: fetchMock,
    });

    assert.equal(decision.admitted, true);
    assert.equal(decision.target, "open-ci");
  });

  it("fails closed when server returns inconsistent admitted/target decision", async () => {
    const fetchMock: typeof fetch = async (input) => {
      const url = String(input);
      if (url.includes("oidc.example")) {
        return Response.json({ value: "signed-jwt-token" });
      }
      return Response.json({
        admitted: true,
        target: "fallback",
        reason: "admitted",
        reservation: RESERVATION,
        runners: { default: `open-ci-linux-x64-${RESERVATION}` },
      });
    };

    const decision = await route({
      endpoint: "https://ci.example.com/v1/ingress/admit",
      audience: "open-ci:acme/app",
      pools: ["default"],
      fallbacks: { default: "ubuntu-latest" },
      runAttempt: 1,
      requestUrl: "https://oidc.example/token",
      requestToken: "req-token",
      fetchImpl: fetchMock,
    });

    assert.equal(decision.admitted, false);
    assert.equal(decision.target, "fallback");
    assert.equal(decision.reason, "admission_unavailable");
  });
});
