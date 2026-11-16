import { randomBytes } from "node:crypto";
import { createSocket } from "node:dgram";
import { isIPv4 } from "node:net";

const address = process.env.DUOLINK_HEALTH_IPV4;
const origin = process.env.DUOLINK_HEALTH_ORIGIN?.replace(/\/$/, "");
if (!address || !isIPv4(address) || !origin?.startsWith("https://")) {
  console.error("Set DUOLINK_HEALTH_IPV4 to your server IPv4 and DUOLINK_HEALTH_ORIGIN to its HTTPS origin.");
  process.exit(2);
}

async function checkHttps() {
  try {
    const response = await fetch(origin + "/healthz", { signal: AbortSignal.timeout(5000) });
    const ok = response.ok && (await response.text()).trim() === "ok";
    console.log("HTTPS /healthz: " + response.status);
    return ok;
  } catch (error) {
    console.log("HTTPS /healthz: " + error.message);
    return false;
  }
}

function checkStun(port) {
  return new Promise(resolve => {
    const socket = createSocket("udp4");
    const request = Buffer.alloc(20);
    request.writeUInt16BE(0x0001);
    request.writeUInt32BE(0x2112a442, 4);
    randomBytes(12).copy(request, 8);
    let retry;
    const deadline = setTimeout(() => finish("timed out"), 2400);
    function finish(error) {
      clearTimeout(deadline);
      clearTimeout(retry);
      socket.close();
      console.log("UDP " + port + ": " + (error || "valid STUN reply"));
      resolve(!error);
    }
    socket.once("error", error => finish(error.message));
    socket.on("message", (reply, remote) => {
      if (remote.address === address && remote.port === port
          && reply.length >= 32 && reply.readUInt16BE(0) === 0x0101
          && reply.subarray(4, 20).equals(request.subarray(4, 20))
          && reply.readUInt16BE(20) === 0x0020) {
        finish();
      }
    });
    socket.send(request, port, address);
    retry = setTimeout(() => socket.send(request, port, address), 800);
  });
}

const results = await Promise.all([checkHttps(), checkStun(3478), checkStun(3479)]);
process.exitCode = results.every(Boolean) ? 0 : 1;
