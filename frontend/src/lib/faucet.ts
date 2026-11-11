// "Get test funds": asks the localnet faucet (services/faucet) to send USDC
// (and a little PURR) to the connected address. URL: ?faucet=… on the page
// URL, else VITE_FAUCET_URL, else http://127.0.0.1:3100.

export const FAUCET_URL: string = (
  new URLSearchParams(location.search).get("faucet") ??
  (import.meta.env.VITE_FAUCET_URL as string | undefined) ??
  "http://127.0.0.1:3100"
).replace(/\/+$/, "");

export type FaucetResult = { ok: true; text: string } | { ok: false; text: string };

export async function requestFunds(address: string): Promise<FaucetResult> {
  let r: Response;
  try {
    r = await fetch(`${FAUCET_URL}/drip`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ address }),
    });
  } catch {
    return { ok: false, text: `Faucet not reachable at ${FAUCET_URL}. Start it with: cd services && npm start` };
  }
  let j: { ok?: boolean; usdc?: number; purr?: number; error?: string; note?: string } = {};
  try {
    j = await r.json();
  } catch {
    /* non-JSON */
  }
  if (r.ok && j.ok) {
    const parts = [`${(j.usdc ?? 0).toLocaleString("en-US")} USDC`];
    if (j.purr) parts.push(`${j.purr.toLocaleString("en-US")} PURR (spot)`);
    return { ok: true, text: `Received ${parts.join(" + ")}${j.note ? `. ${j.note}` : ""}` };
  }
  return { ok: false, text: j.error ?? `faucet answered HTTP ${r.status}` };
}
