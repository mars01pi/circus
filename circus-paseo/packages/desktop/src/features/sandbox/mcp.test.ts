import { expect, test } from "vitest";
import { probeGithub } from "./mcp.js";

const input = {
  id: "github",
  url: "https://mcp.example/mcp/",
  token: "fixture-token",
  owner: "approved-team",
  repo: "approved-repo",
};
const tool = {
  name: "issue_read",
  inputSchema: {
    type: "object",
    properties: { method: {}, owner: {}, repo: {}, issue_number: {} },
  },
};

test("probes discovery without invoking tools, follows bounded pagination, and closes its session", async () => {
  const requests: RequestInit[] = [];
  const request: typeof fetch = async (url, init) => {
    expect(url).toBe(input.url);
    expect(init?.redirect).toBe("error");
    if (init) requests.push(init);
    if (init?.method === "DELETE") return new Response(null, { status: 204 });
    const rpc = JSON.parse(String(init?.body));
    if (rpc.method === "notifications/initialized") return new Response(null, { status: 202 });
    let result: unknown;
    if (rpc.method === "initialize")
      result = { protocolVersion: "2025-03-26", capabilities: { tools: {} } };
    else result = rpc.params.cursor ? { tools: [tool] } : { tools: [], nextCursor: "next" };
    return new Response(JSON.stringify({ jsonrpc: "2.0", id: rpc.id, result }), {
      headers: { "mcp-session-id": "session", "Content-Type": "application/json" },
    });
  };
  expect(await probeGithub(input, request)).toMatchObject({ ok: true });
  expect(requests).toHaveLength(5);
  expect(requests.at(-1)?.method).toBe("DELETE");
  expect(requests.map((item) => item.body).join()).not.toContain("tools/call");
  expect(requests.map((item) => item.body).join()).not.toContain(input.token);
});

test("accepts a streamed discovery envelope and does not expose service errors", async () => {
  const request: typeof fetch = async (_url, init) => {
    const rpc = JSON.parse(String(init?.body));
    if (rpc.method === "notifications/initialized") return new Response(null, { status: 202 });
    const result =
      rpc.method === "initialize"
        ? { protocolVersion: "2025-03-26", capabilities: { tools: {} } }
        : { tools: [tool] };
    return new Response(
      `event: message\ndata: ${JSON.stringify({ jsonrpc: "2.0", id: rpc.id, result })}\n\n`,
      { headers: { "Content-Type": "text/event-stream" } },
    );
  };
  expect(await probeGithub(input, request)).toMatchObject({ ok: true });
  const refused: typeof fetch = async () =>
    new Response("sensitive provider diagnostics", { status: 403 });
  const failure = await probeGithub(input, refused);
  expect(failure.ok).toBe(false);
  expect(JSON.stringify(failure)).not.toContain("sensitive");
});

test("rejects unsupported tools instead of claiming that a preset is connected", async () => {
  const request: typeof fetch = async (_url, init) => {
    const rpc = JSON.parse(String(init?.body));
    if (rpc.method === "notifications/initialized") return new Response(null, { status: 202 });
    const result =
      rpc.method === "initialize"
        ? { protocolVersion: "2025-03-26", capabilities: { tools: {} } }
        : { tools: [{ ...tool, name: "delete_repository" }] };
    return new Response(JSON.stringify({ jsonrpc: "2.0", id: rpc.id, result }));
  };
  expect(await probeGithub(input, request)).toMatchObject({ ok: false });
});

function discoveryRequest(initialization: unknown, listing: (page: number) => unknown) {
  let pages = 0;
  const request: typeof fetch = async (_url, init) => {
    const rpc = JSON.parse(String(init?.body));
    if (rpc.method === "notifications/initialized") return new Response(null, { status: 202 });
    const result = rpc.method === "initialize" ? initialization : listing(pages++);
    return new Response(JSON.stringify({ jsonrpc: "2.0", id: rpc.id, result }));
  };
  return { request, pages: () => pages };
}
const initialization = { protocolVersion: "2025-03-26", capabilities: { tools: {} } };

test.each([
  { protocolVersion: "2024-11-05", capabilities: { tools: {} } },
  { protocolVersion: "2025-03-26", capabilities: {} },
])("rejects unsupported protocol or missing tools capability before discovery", async (reply) => {
  const service = discoveryRequest(reply, () => ({ tools: [tool] }));
  expect(await probeGithub(input, service.request)).toMatchObject({ ok: false });
  expect(service.pages()).toBe(0);
});

test.each([
  { ...tool, inputSchema: { ...tool.inputSchema, required: ["extra_permission"] } },
  { ...tool, inputSchema: { ...tool.inputSchema, type: "array" } },
])("rejects tool schemas that cannot be served by the narrow preset", async (unsupported) => {
  const service = discoveryRequest(initialization, () => ({ tools: [unsupported] }));
  expect(await probeGithub(input, service.request)).toMatchObject({ ok: false });
});

test("bounds pagination and rejects repeated cursors without calling tools", async () => {
  const repeating = discoveryRequest(initialization, () => ({ tools: [], nextCursor: "same" }));
  expect(await probeGithub(input, repeating.request)).toMatchObject({ ok: false });
  expect(repeating.pages()).toBe(2);
  const endless = discoveryRequest(initialization, (page) => ({
    tools: [],
    nextCursor: String(page),
  }));
  expect(await probeGithub(input, endless.request)).toMatchObject({ ok: false });
  expect(endless.pages()).toBe(4);
  const tooMany = discoveryRequest(initialization, () => ({
    tools: Array.from({ length: 300 }, () => ({ ...tool, name: "other" })),
    nextCursor: "more",
  }));
  expect(await probeGithub(input, tooMany.request)).toMatchObject({ ok: false });
  expect(tooMany.pages()).toBe(2);
});
