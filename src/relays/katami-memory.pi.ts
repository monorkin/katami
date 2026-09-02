// installed by agent — managed file; edits are overwritten on the next launch.
// KATAMI_RELAY_VERSION=1
// @ts-nocheck
//
// Relays pi session events to the agent supervisor over its unix socket and
// injects the memories it returns. Inert unless KATAMI_HOOK_SOCKET is set, so a
// stray copy outside a supervised session does nothing.

import net from "node:net";

const socketPath = process.env.KATAMI_HOOK_SOCKET;

function ask(event, payload, timeoutMs = 1500) {
  if (!socketPath) return Promise.resolve(null);
  return new Promise((resolve) => {
    let buffer = "";
    const socket = net.createConnection(socketPath);
    const finish = (value) => {
      socket.destroy();
      resolve(value);
    };
    socket.setTimeout(timeoutMs, () => finish(null));
    socket.on("error", () => finish(null));
    socket.on("connect", () =>
      socket.write(JSON.stringify({ event, tool: "pi", payload }) + "\n"));
    socket.on("data", (chunk) => {
      buffer += chunk;
      const newline = buffer.indexOf("\n");
      if (newline >= 0) {
        try {
          finish(JSON.parse(buffer.slice(0, newline)));
        } catch {
          finish(null);
        }
      }
    });
    socket.on("end", () => finish(null));
  });
}

export default function (pi) {
  if (!socketPath) return;
  let pendingContext = null;

  const base = (ctx) => ({
    session_id: ctx?.sessionManager?.getSessionId?.(),
    transcript_path: ctx?.sessionManager?.getSessionFile?.(),
    cwd: ctx?.cwd,
  });

  // pi's only injection channel is before_agent_start, so SessionStart context
  // is stashed and delivered with the first prompt.
  pi.on("session_start", async (_event, ctx) => {
    const reply = await ask("SessionStart", base(ctx));
    pendingContext = reply?.context ?? null;
  });

  pi.on("before_agent_start", async (event, ctx) => {
    const reply = await ask("UserPromptSubmit", { ...base(ctx), prompt: event?.prompt ?? "" });
    const pieces = [pendingContext, reply?.context].filter(Boolean);
    pendingContext = null;
    if (!pieces.length) return;
    return { message: { customType: "katami-memory", content: pieces.join("\n\n"), display: false } };
  });

  pi.on("agent_end", (_event, ctx) => {
    void ask("Stop", base(ctx));
  });

  pi.on("session_shutdown", (event, ctx) =>
    ask("SessionEnd", { ...base(ctx), reason: event?.reason }));
}
