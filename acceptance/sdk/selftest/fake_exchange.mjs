// Fake /exchange endpoint for ts_harness_selftest.sh (see there).
import { createServer } from "node:http";

const port = Number(process.argv[2]);
createServer((req, res) => {
  let body = "";
  req.on("data", (c) => (body += c));
  req.on("end", () => {
    let type = "";
    try {
      type = JSON.parse(body)?.action?.type ?? "";
    } catch {}
    let out;
    if (type === "withdraw3") out = { status: "err", response: "Withdrawal amount must be greater than the withdrawal fee." };
    else if (type === "usdSend") out = { status: "err", response: "Some other error" };
    else out = { status: "ok", response: { type: "default" } };
    res.writeHead(200, { "content-type": "application/json" });
    res.end(JSON.stringify(out));
  });
}).listen(port, "127.0.0.1");
