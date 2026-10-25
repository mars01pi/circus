import { z } from "zod";
import type { SandboxGithubInput, SandboxSetupResult } from "@getpaseo/protocol/sandbox-setup";

const envelopeSchema = z.object({
  jsonrpc: z.literal("2.0"),
  id: z.number(),
  result: z.record(z.string(), z.unknown()).optional(),
  error: z.unknown().optional(),
});
const initializationSchema = z.object({
  protocolVersion: z.literal("2025-03-26"),
  capabilities: z.object({ tools: z.record(z.string(), z.unknown()) }),
});
const toolsSchema = z.object({
  tools: z
    .array(
      z.object({
        name: z.string(),
        inputSchema: z.object({
          type: z.literal("object"),
          properties: z.record(z.string(), z.unknown()),
          required: z.array(z.string()).max(32).optional(),
        }),
      }),
    )
    .max(512),
  nextCursor: z.string().max(1024).optional(),
});

function supportsIssueRead(tool: z.infer<typeof toolsSchema>["tools"][number]): boolean {
  const expected = ["method", "owner", "repo", "issue_number"];
  return (
    expected.every((name) => Object.hasOwn(tool.inputSchema.properties, name)) &&
    !tool.inputSchema.required?.some((name) => !expected.includes(name))
  );
}

function streamedEnvelope(blocks: string[], id: number) {
  for (const block of blocks) {
    const data = block
      .split(/\r?\n/)
      .filter((line) => line.startsWith("data:"))
      .map((line) => line.slice(5).trimStart())
      .join("\n");
    if (!data) continue;
    const parsed = envelopeSchema.safeParse(JSON.parse(data));
    if (parsed.success && parsed.data.id === id) return parsed.data;
  }
  return null;
}

async function readEnvelope(response: Response, id: number) {
  const reader = response.body?.getReader();
  if (!reader) return null;
  const decoder = new TextDecoder();
  let text = "";
  let length = 0;
  const sse = response.headers.get("content-type")?.includes("text/event-stream");
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      length += value.byteLength;
      if (length > 1024 * 1024) return null;
      text += decoder.decode(value, { stream: true });
      if (sse) {
        const blocks = text.split(/\r?\n\r?\n/);
        text = blocks.pop() ?? "";
        const envelope = streamedEnvelope(blocks, id);
        if (envelope) return envelope;
      }
    }
    if (sse) return null;
    text += decoder.decode();
    const parsed = envelopeSchema.safeParse(JSON.parse(text));
    return parsed.success && parsed.data.id === id ? parsed.data : null;
  } finally {
    await reader.cancel();
  }
}

/** A discovery probe, never a tool invocation. The same URL owns the session and credential. */
export async function probeGithub(
  input: SandboxGithubInput,
  request: typeof fetch,
): Promise<SandboxSetupResult> {
  let session: string | null = null;
  const signal = AbortSignal.timeout(20_000);
  const headers: Record<string, string> = {
    Authorization: `Bearer ${input.token}`,
    "Content-Type": "application/json",
    Accept: "application/json, text/event-stream",
  };
  async function send(body: unknown, id: number | null) {
    const response = await request(input.url, {
      method: "POST",
      redirect: "error",
      signal,
      headers,
      body: JSON.stringify(body),
    });
    if (session === null) {
      const received = response.headers.get("mcp-session-id");
      if (received && received.length <= 256 && !/[\r\n]/.test(received)) {
        session = received;
        headers["Mcp-Session-Id"] = received;
      }
    }
    if (!response.ok) {
      await response.body?.cancel();
      return null;
    }
    if (id === null) {
      await response.body?.cancel();
      return {};
    }
    return await readEnvelope(response, id);
  }
  try {
    const initialized = await send(
      {
        jsonrpc: "2.0",
        id: 1,
        method: "initialize",
        params: {
          protocolVersion: "2025-03-26",
          capabilities: {},
          clientInfo: { name: "circus-desktop-setup", version: "1" },
        },
      },
      1,
    );
    if (
      !initialized ||
      !("result" in initialized) ||
      !initializationSchema.safeParse(initialized.result).success
    )
      return {
        ok: false,
        message:
          "MCP initialization failed. Check the HTTPS endpoint and token permissions. Only Streamable HTTP is supported.",
      };
    headers["MCP-Protocol-Version"] = "2025-03-26";
    if (!(await send({ jsonrpc: "2.0", method: "notifications/initialized" }, null)))
      return { ok: false, message: "The MCP service could not start a session." };
    let cursor: string | undefined;
    let toolCount = 0;
    const cursors = new Set<string>();
    for (let page = 0; page < 4; page += 1) {
      const id = page + 2;
      const listing = await send(
        { jsonrpc: "2.0", id, method: "tools/list", params: cursor ? { cursor } : {} },
        id,
      );
      if (!listing || !("result" in listing)) break;
      const parsed = toolsSchema.safeParse(listing.result);
      if (!parsed.success) break;
      toolCount += parsed.data.tools.length;
      if (toolCount > 512) break;
      const issue = parsed.data.tools.find((tool) => tool.name === "issue_read");
      if (issue) {
        if (!supportsIssueRead(issue)) break;
        return {
          ok: true,
          message:
            "The service authenticated and advertises GitHub issue_read. The saved policy permits reading issues only in the repository you chose. Repository access is checked when the tool is used.",
        };
      }
      cursor = parsed.data.nextCursor;
      if (!cursor || cursors.has(cursor)) break;
      cursors.add(cursor);
    }
    return {
      ok: false,
      message:
        "The service did not advertise the expected issue_read tool. This preset requires the GitHub MCP issue_read schema; no connection was saved.",
    };
  } catch {
    return {
      ok: false,
      message:
        "Could not verify the MCP connection. Check its HTTPS address, trusted certificate and token. Redirects are not followed; no raw service response is shown.",
    };
  } finally {
    if (session) {
      try {
        const response = await request(input.url, {
          method: "DELETE",
          redirect: "error",
          signal: AbortSignal.timeout(3000),
          headers,
        });
        await response.body?.cancel();
      } catch {
        /* Discovery sessions may be stateless or expire without DELETE support. */
      }
    }
  }
}
