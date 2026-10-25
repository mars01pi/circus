import { afterEach, describe, expect, it } from "vitest";
import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { CircusStdioClient, type CircusTaskEvent } from "./stdio-client.js";

const roots: string[] = [];
const clients: CircusStdioClient[] = [];
afterEach(async () => {
  await Promise.allSettled(clients.splice(0).map((client) => client.close()));
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

async function peer(mode: string): Promise<CircusStdioClient> {
  const root = await mkdtemp(join(tmpdir(), "circus-stdio-"));
  roots.push(root);
  const executable = join(root, "peer");
  const script = join(root, "peer.mjs");
  await writeFile(
    executable,
    `#!/usr/bin/env bash\nexec '${process.execPath.replaceAll("'", "'\\''")}' '${script.replaceAll("'", "'\\''")}' "$@"\n`,
    { mode: 0o700 },
  );
  await writeFile(
    script,
    `
import {createInterface} from 'node:readline';
const mode = process.env.PEER_MODE;
const send = frame => process.stdout.write(JSON.stringify({v:1,...frame})+'\\n');
send({type:'ready',maxFrameBytes:12582912});
let correlation, ended = false;
const finish = () => {
  ended = true;
  send({...correlation,type:'result',result:{status:'cancelled',cleanup:'confirmed',error:{code:'cancelled',message:'Task cancelled'}}});
};
createInterface({input:process.stdin}).on('line', line => {
  const request = JSON.parse(line);
  if(request.type === 'start') {
    correlation = {requestId:request.requestId,taskId:'task-owned'};
    send({...correlation,type:'accepted'});
    if(mode === 'disconnect') return process.exit(3);
    if(mode === 'partial') { process.stdout.write('{'); return process.exit(0); }
    if(mode === 'wrong-task') return send({...correlation,taskId:'other-client',type:'event',seq:1,event:{kind:'phase',phase:'running'}});
    if(mode === 'overflow') return process.stdout.write('x'.repeat(12582913));
    if(mode === 'wait') return;
    if(mode === 'hang') { setInterval(() => {}, 1000); return; }
    if(mode === 'event-flood') {
      for(let seq=1; seq<=257; seq++) send({...correlation,type:'event',seq,event:{kind:'text_delta',text:'x'}});
      return;
    }
    send({...correlation,type:'event',seq:1,event:{kind:'text_delta',text:'hello'}});
    ended = true;
    if(mode === 'cleanup') return send({...correlation,type:'result',result:{status:'failed',cleanup:'pending',error:{code:'cleanup',message:'Cleanup failed'}}});
    send({...correlation,type:'result',result:{status:'succeeded',cleanup:'confirmed',artifacts:mode === 'missing-artifact' ? [] : request.request.inputFiles}});
  } else if(request.type === 'cancel') { send({...correlation,type:'cancel_ack'}); finish(); }
  else if(request.type === 'cleanup_retry') send({...correlation,type:'cleanup_result',cleanup:'confirmed'});
}).on('close', () => { if(correlation && !ended && mode === 'wait') finish(); });
`,
  );
  const client = new CircusStdioClient({
    executable,
    configFile: join(root, "config.json"),
    stateDir: join(root, "state"),
    env: { PEER_MODE: mode, PATH: "/usr/bin:/bin" },
    shutdownTimeoutMs: 1000,
  });
  clients.push(client);
  return client;
}

const request = {
  prompt: "Fix file",
  inputFiles: { "answer.txt": Buffer.from([0, 255, 42]) },
  outputPaths: ["answer.txt"],
  timeoutMs: 1000,
};

// The peer is a real subprocess protocol adapter; these tests make no sandbox claims.
describe.skipIf(process.platform === "win32")("stdio client subprocess contract", () => {
  it("streams correlated events and returns exact binary artifacts", async () => {
    const client = await peer("success");
    const task = await client.start(request);
    const events: CircusTaskEvent[] = [];
    for await (const event of task.events) events.push(event);
    expect(events).toEqual([{ kind: "text_delta", text: "hello" }]);
    expect(await task.result).toEqual({
      status: "succeeded",
      cleanup: "confirmed",
      artifacts: request.inputFiles,
    });
    await client.close();
    await expect(client.start(request)).rejects.toMatchObject({ code: "closed" });
  });

  it("acknowledges cancellation but waits for authoritative cleanup result", async () => {
    const client = await peer("wait");
    const task = await client.start(request);
    await task.cancel();
    expect(await task.result).toEqual({
      status: "cancelled",
      cleanup: "confirmed",
      error: { code: "cancelled", message: "Task cancelled" },
    });
    await client.close();
  });

  it("cancels through EOF when the client closes", async () => {
    const client = await peer("wait");
    const task = await client.start(request);
    await client.close();
    expect(await task.result).toMatchObject({ status: "cancelled", cleanup: "confirmed" });
  });

  it("retains cleanup ownership for explicit retry", async () => {
    const client = await peer("cleanup");
    const task = await client.start(request);
    expect(await task.result).toMatchObject({ status: "failed", cleanup: "pending" });
    expect(await task.retryCleanup()).toBe("confirmed");
    await client.close();
  });

  it.each(["disconnect", "partial", "wrong-task", "overflow", "event-flood", "missing-artifact"])(
    "does not report success for %s",
    async (mode) => {
      const client = await peer(mode);
      const task = await client.start(request);
      await expect(task.result).rejects.toMatchObject({ cleanup: "unknown" });
      await expect(client.close()).rejects.toMatchObject({ cleanup: "unknown" });
    },
  );

  it("reports unknown cleanup when forced shutdown is needed", async () => {
    const client = await peer("hang");
    const task = await client.start(request);
    await expect(client.close()).rejects.toMatchObject({ code: "timeout", cleanup: "unknown" });
    await expect(task.result).rejects.toMatchObject({ cleanup: "unknown" });
  });

  it("rejects invalid requests before launching", async () => {
    const client = await peer("success");
    await expect(client.start({ ...request, outputPaths: ["../../secret"] })).rejects.toMatchObject(
      { code: "invalid_request" },
    );
  });
});
