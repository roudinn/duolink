import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { createSocket } from "node:dgram";
import { once } from "node:events";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import * as http from "node:http";
import * as https from "node:https";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import { setTimeout as sleep } from "node:timers/promises";

const serverBinary = process.argv[2];
if (!serverBinary || process.argv.length !== 3) {
  throw new Error("Usage: node server/tests/capacity.mjs <duolink-server executable>");
}
const caPath = process.env.DUOLINK_CAPACITY_CA;
const transport = caPath ? https : http;
const tls = caPath ? {
  ca: readFileSync(caPath),
  servername: "link.example.net",
} : {};

const state = mkdtempSync(join(tmpdir(), "duolink-capacity-"));
const server = spawn(resolve(serverBinary), [], {
  env: {
    ...process.env,
    DUOLINK_STATE_DIR: state,
    DUOLINK_HTTP: caPath ? "127.0.0.1:8705" : "127.0.0.1:0",
    DUOLINK_STUN_A: "127.0.0.1:0",
    DUOLINK_STUN_B: "127.0.0.1:0",
  },
  stdio: ["ignore", "ignore", "pipe"],
  windowsHide: true,
});
const lines = createInterface({ input: server.stderr });
const clients = Array.from({ length: 100 }, (_, index) => index + 1);
const sockets = new Set();
const agent = new transport.Agent({ keepAlive: true, maxSockets: 120 });
let httpPort;

function topic(kind, client) {
  const key = client.toString(16).padStart(32, "0");
  return "/signal/duolink-" + kind + key;
}

function open(path, { body, stream = false } = {}) {
  return new Promise((resolve, reject) => {
    const req = transport.request({
      host: "127.0.0.1", port: httpPort, path,
      agent: stream ? false : agent, method: body ? "POST" : "GET",
      ...tls,
    }, response => {
      if (stream && response.statusCode === 200) {
        req.setTimeout(0);
        sockets.add(response);
        response.once("close", () => sockets.delete(response));
        resolve(response);
        return;
      }
      response.setEncoding("utf8");
      let text = "";
      response.on("data", chunk => { text += chunk; });
      response.once("end", () => resolve({ status: response.statusCode, text }));
      response.once("error", reject);
    });
    req.setTimeout(4500, () => req.destroy(new Error("HTTP request timed out")));
    req.once("error", reject);
    req.end(body);
  });
}

async function burst(label, operation) {
  const durations = [];
  const results = await Promise.allSettled(clients.map(async client => {
    const started = performance.now();
    const value = await operation(client);
    durations.push(performance.now() - started);
    return value;
  }));
  const errors = results.filter(result => result.status === "rejected");
  assert.equal(errors.length, 0, label + ": " + errors.length + " failed; " + errors[0]?.reason);
  durations.sort((a, b) => a - b);
  console.log(`${label}: ${clients.length}/${clients.length}, p95 ${durations[94].toFixed(1)} ms`);
  return results.map(result => result.value);
}

async function subscribe(path, previousId = 0) {
  const response = await open(path + "/json?since=0", { stream: true });
  assert.equal(response.statusCode ?? response.status, 200, "subscription rejected");
  const reader = createInterface({ input: response });
  let lastId = previousId;
  const received = [];
  let heartbeats = 0;
  reader.on("line", line => {
    if (!line) {
      heartbeats++;
      return;
    }
    const record = JSON.parse(line);
    if (record.id > lastId) {
      lastId = record.id;
      received.push(record.message);
    }
  });
  return {
    received,
    get lastId() { return lastId; },
    get heartbeats() { return heartbeats; },
    close: () => response.destroy(),
  };
}

async function send(path, body) {
  const response = await open(path, { body });
  assert.equal(response.status, 204, "message rejected");
}

function stun(socket, port) {
  return new Promise((resolve, reject) => {
    const request = Buffer.alloc(20);
    request.writeUInt16BE(0x0001);
    request.writeUInt32BE(0x2112a442, 4);
    randomBytes(12).copy(request, 8);
    const onMessage = (response, remote) => {
      if (remote.port !== port || !response.subarray(8, 20).equals(request.subarray(8, 20))) return;
      clearTimeout(timer);
      socket.off("message", onMessage);
      resolve(response);
    };
    const timer = setTimeout(() => {
      socket.off("message", onMessage);
      reject(new Error("missing STUN reply on " + port));
    }, 3000);
    socket.on("message", onMessage);
    socket.send(request, port, "127.0.0.1");
  });
}

try {
  const [line] = await Promise.race([
    once(lines, "line", { signal: AbortSignal.timeout(10000) }),
    once(server, "exit").then(([code]) => { throw new Error("Server exited with " + code); }),
  ]);
  const match = /^DuoLink listening on 127\.0\.0\.1:(\d+), STUN 127\.0\.0\.1:(\d+) and 127\.0\.0\.1:(\d+)$/.exec(line);
  assert.ok(match, line);
  const [http, stunA, stunB] = match.slice(1).map(Number);
  httpPort = caPath ? 19443 : http;
  let inboxes;
  [inboxes] = await Promise.all([
    burst("Open friend inboxes", client => subscribe(topic("i-", client))),
    burst("Publish and immediately look up nicknames", async client => {
      const path = topic("d-", client);
      await send(path, "key" + client);
      const response = await open(path + "/json?poll=1&since=all");
      assert.equal(response.status, 200, "lookup rejected");
      assert.equal(JSON.parse(response.text).message, "key" + client);
    }),
  ]);

  const lobbies = await burst("Open lobbies while all inboxes are online", client => subscribe(topic("", client)));
  await Promise.all([
    burst("Send friend requests", client => send(topic("i-", client), "request" + client)),
    burst("Send lobby rendezvous messages", client => send(topic("", client), "offer" + client)),
    burst("Probe both STUN ports", async () => {
      const socket = createSocket("udp4");
      try {
        const replies = await Promise.all([stun(socket, stunA), stun(socket, stunB)]);
        for (const reply of replies) assert.equal(reply.readUInt16BE(), 0x0101);
      } finally {
        socket.close();
      }
    }),
  ]);
  await waitForDelivery();
  for (const [index, client] of clients.entries()) {
    assert.deepEqual(inboxes[index].received, ["request" + client]);
    assert.deepEqual(lobbies[index].received, ["offer" + client]);
  }

  const cursors = inboxes.map(inbox => inbox.lastId);
  inboxes.forEach(inbox => inbox.close());
  await burst("Store mail while inboxes reconnect", client => send(topic("i-", client), "accept" + client));
  inboxes = await burst("Reconnect all inboxes", client => subscribe(topic("i-", client), cursors[client - 1]));
  await waitForDelivery();
  for (const [index, client] of clients.entries()) {
    assert.deepEqual(inboxes[index].received, ["accept" + client]);
  }
  assert.equal((await open("/healthz")).status, 200);
  console.log("PASS 100 simultaneous users, 200 streams, friend and session delivery, STUN and mail replay");

  async function waitForDelivery() {
    for (let attempt = 0; attempt < 40; attempt++) {
      if ([...inboxes, ...lobbies].every(subscription => subscription.received.length > 0)) return;
      await sleep(50);
    }
    throw new Error("Delivery did not complete");
  }
} finally {
  for (const socket of sockets) socket.destroy();
  agent.destroy();
  const exited = once(server, "exit");
  if (server.kill()) await exited;
  lines.close();
  rmSync(state, { recursive: true, force: true });
}
