// installed by agent — managed file; edits are overwritten on the next launch.
// KATAMI_RELAY_VERSION=1
//
// Relays opencode chat events to the agent supervisor over its unix socket and
// injects the memories it returns as synthetic message parts. Inert unless
// KATAMI_HOOK_SOCKET is set.

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
      socket.write(JSON.stringify({ event, tool: "opencode", payload }) + "\n"));
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

function partId() {
  return `prt_agmem${Date.now().toString(36)}${Math.random().toString(36).slice(2, 10)}`;
}

export const AgentMemoryPlugin = async ({ directory }) => {
  if (!socketPath) return {};
  const greeted = new Set();
  const children = new Set();

  return {
    "chat.message": async (input, output) => {
      const sessionID = input?.sessionID ?? output?.message?.sessionID;
      if (!sessionID || children.has(sessionID)) return;

      const prompt = (output.parts ?? [])
        .filter((part) => part.type === "text" && !part.synthetic)
        .map((part) => part.text)
        .join("\n");

      const pieces = [];
      if (!greeted.has(sessionID)) {
        greeted.add(sessionID);
        const start = await ask("SessionStart", { session_id: sessionID, cwd: directory });
        if (start?.context) pieces.push(start.context);
      }
      const reply = await ask("UserPromptSubmit", { session_id: sessionID, cwd: directory, prompt });
      if (reply?.context) pieces.push(reply.context);

      for (const text of pieces) {
        // Push, never reassign — opencode re-reads this same array after the
        // hook, but ignores a replaced binding.
        output.parts.push({
          id: partId(),
          sessionID,
          messageID: output.message.id,
          type: "text",
          text,
          synthetic: true,
          metadata: { katamiMemory: true },
        });
      }
    },

    event: async ({ event }) => {
      const properties = event?.properties ?? {};
      const info = properties.info;
      // Subagent sessions carry a parentID — their turns aren't the user's.
      if (info?.id && info.parentID) children.add(info.id);
      if (event?.type === "session.idle" && properties.sessionID && !children.has(properties.sessionID)) {
        await ask("Stop", { session_id: properties.sessionID, cwd: directory });
      }
    },
  };
};
