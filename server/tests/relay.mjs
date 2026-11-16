import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { once } from "node:events";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";

const target = process.argv[2];
assert.ok(target, "Pass a server executable path or a WebSocket relay URL");
const local = !target.startsWith("wss://");
const state = local ? mkdtempSync(join(tmpdir(), "duolink-relay-")) : null;
const server = local ? spawn(resolve(target), [], {
  env: {
    ...process.env,
    DUOLINK_STATE_DIR: state,
    DUOLINK_HTTP: "127.0.0.1:0",
    DUOLINK_STUN_A: "127.0.0.1:0",
    DUOLINK_STUN_B: "127.0.0.1:0",
  },
  stdio: ["ignore", "ignore", "pipe"],
  windowsHide: true,
}) : null;
const clients = new Set();

function receive(socket) {
  return new Promise((resolveMessage, reject) => {
    const controller = new AbortController();
    const options = { signal: controller.signal, once: true };
    const timeout = setTimeout(() => done(reject, new Error("Relay response timed out")), 4000);
    function done(callback, value) {
      clearTimeout(timeout);
      controller.abort();
      callback(value);
    }
    socket.addEventListener("message", ({ data }) => done(resolveMessage, Buffer.from(data)), options);
    socket.addEventListener("close", () => done(reject, new Error("Relay disconnected")), options);
    socket.addEventListener("error", () => done(reject, new Error("Relay socket error")), options);
  });
}

let url = target;
async function register(role, key, expected = "ready") {
  const socket = new WebSocket(url);
  clients.add(socket);
  socket.binaryType = "arraybuffer";
  socket.addEventListener("open", () => socket.send(Buffer.concat([Buffer.from([role]), key])), { once: true });
  assert.equal((await receive(socket)).toString(), expected);
  return socket;
}

let lines;
try {
  if (server) {
    lines = createInterface({ input: server.stderr });
    const [line] = await once(lines, "line", { signal: AbortSignal.timeout(5000) });
    const match = /^DuoLink listening on 127\.0\.0\.1:(\d+),/.exec(line);
    assert.ok(match, line);
    url = "ws://127.0.0.1:" + match[1] + "/relay";
  }
  const keys = Array.from({ length: local ? 20 : 2 }, () => randomBytes(32));
  const hosts = await Promise.all(keys.map(key => register(0, key)));
  const guests = await Promise.all(keys.slice(0, local ? 5 : 1).map(key => register(1, key)));
  if (local) {
    await register(1, keys[5], "RelayFull");
    const extra = await register(1, keys[0]);
    const removed = receive(hosts[0]);
    extra.close();
    assert.equal((await removed).length, 4);
  }
  const packet = receive(hosts[0]);
  guests[0].send(Buffer.from("input"));
  const data = await packet;
  assert.equal(data.subarray(4).toString(), "input");
  const reply = receive(guests[0]);
  hosts[0].send(Buffer.concat([data.subarray(0, 4), Buffer.from("video")]));
  assert.equal((await reply).toString(), "video");

  if (local) {
    const removed = receive(hosts[4]);
    guests[4].close();
    assert.equal((await removed).length, 4);
    await register(1, keys[5]);
    console.log("PASS: 20 waiting lobbies, five active rooms, RelayFull and slot reuse");
  }
  console.log("PASS: relay payload round-trip");
} finally {
  for (const socket of clients) socket.close();
  if (server) {
    const exited = once(server, "exit");
    if (server.kill()) await exited;
    lines?.close();
    rmSync(state, { recursive: true, force: true });
  }
}
