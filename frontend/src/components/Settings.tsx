import { useEffect, useRef, useState } from "react";
import { privateKeyToAccount } from "viem/accounts";
import { DEFAULT_API_URL, normalizeApiUrl, normalizeKey } from "../lib/settings";

interface LocalAccount {
  address: string;
  secret: string;
  name?: string;
}

interface Props {
  apiUrl: string;
  currentKey: string | null;
  remember: boolean;
  onApply: (apiUrl: string, key: `0x${string}` | null, remember: boolean) => void;
  onClose: () => void;
}

export function Settings({ apiUrl, currentKey, remember: remember0, onApply, onClose }: Props) {
  const [url, setUrl] = useState(apiUrl);
  const [key, setKey] = useState(currentKey ?? "");
  const [remember, setRemember] = useState(remember0);
  const [accounts, setAccounts] = useState<LocalAccount[] | null>(null);
  const [acctNote, setAcctNote] = useState<string>("");
  const fileRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    // Dev-server helper (vite.config.ts) serving ../run/localnet/accounts.json.
    fetch("/__localnet/accounts.json", { cache: "no-store" })
      .then(async (r) => {
        const j = await r.json();
        if (!r.ok) throw new Error(j.error ?? r.statusText);
        setAccounts(parseAccounts(j));
        setAcctNote("from run/localnet/accounts.json (dev server)");
      })
      .catch((e) => setAcctNote(`localnet accounts not found (${e.message}); use the file picker or paste a key`));
  }, []);

  const loadFile = async (f: File) => {
    try {
      setAccounts(parseAccounts(JSON.parse(await f.text())));
      setAcctNote(`from ${f.name}`);
    } catch (e) {
      setAcctNote(`could not parse ${f.name}: ${(e as Error).message}`);
    }
  };

  const nk = key.trim() ? normalizeKey(key) : null;
  let address = "";
  if (nk) {
    try {
      address = privateKeyToAccount(nk).address;
    } catch {
      /* invalid */
    }
  }
  const keyInvalid = key.trim() !== "" && !address;

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()} data-testid="settings">
        <div className="modal-head">
          <h2>Settings</h2>
          <button className="icon-btn" onClick={onClose} aria-label="Close">
            ×
          </button>
        </div>

        <label className="field">
          <span>API base URL</span>
          <input data-testid="api-url" value={url} onChange={(e) => setUrl(e.target.value)} placeholder={DEFAULT_API_URL} />
          <small>
            HTTP <code>/info</code>, <code>/exchange</code>; WebSocket at <code>{wsHint(url)}</code>. Default {DEFAULT_API_URL}.
          </small>
        </label>

        <div className="field">
          <span>Funded localnet accounts</span>
          <div className="row gap">
            <select
              data-testid="account-select"
              value=""
              onChange={(e) => {
                const a = accounts?.find((x) => x.address === e.target.value);
                if (a) setKey(a.secret);
              }}
              disabled={!accounts?.length}
            >
              <option value="">{accounts?.length ? "Pick an account…" : "none loaded"}</option>
              {accounts?.map((a, i) => (
                <option key={a.address} value={a.address}>
                  {a.name ?? `account ${i}`} · {short(a.address)}
                </option>
              ))}
            </select>
            <button className="btn" onClick={() => fileRef.current?.click()}>
              Load accounts.json…
            </button>
            <input
              ref={fileRef}
              type="file"
              accept=".json,application/json"
              hidden
              onChange={(e) => e.target.files?.[0] && loadFile(e.target.files[0])}
            />
          </div>
          <small>{acctNote}</small>
        </div>

        <label className="field">
          <span>Private key</span>
          <input
            data-testid="private-key"
            type="password"
            autoComplete="off"
            spellCheck={false}
            value={key}
            onChange={(e) => setKey(e.target.value)}
            placeholder="0x… (64 hex chars)"
          />
          <small className={keyInvalid ? "neg" : ""}>
            {keyInvalid ? "not a valid 32-byte hex key" : address ? `address ${address}` : "no key: read-only mode"}
          </small>
        </label>

        <label className="check">
          <input type="checkbox" checked={remember} onChange={(e) => setRemember(e.target.checked)} />
          Remember key in this browser (localStorage)
        </label>

        <p className="warn">
          Local dev tool. The key never leaves this browser except as signatures on the actions you submit; it is held in
          memory, and in localStorage only if you tick the box above. Do not paste a key that holds real funds.
        </p>

        <div className="row gap end">
          <button className="btn" onClick={() => setKey("")}>
            Clear key
          </button>
          <button
            className="btn btn-primary"
            data-testid="settings-apply"
            disabled={keyInvalid}
            onClick={() => onApply(normalizeApiUrl(url || DEFAULT_API_URL), nk, remember)}
          >
            Apply
          </button>
        </div>
      </div>
    </div>
  );
}

function parseAccounts(j: unknown): LocalAccount[] {
  if (!Array.isArray(j)) throw new Error("expected a JSON array of {address, secret}");
  return j
    .filter((x) => x && typeof x.secret === "string" && typeof x.address === "string")
    .map((x) => ({ address: x.address, secret: x.secret, name: x.name }))
    .sort((a, b) => (a.name === "sdk_example" ? -1 : b.name === "sdk_example" ? 1 : 0));
}

function wsHint(u: string) {
  try {
    return normalizeApiUrl(u || DEFAULT_API_URL).replace(/^http/, "ws") + "/ws";
  } catch {
    return "";
  }
}

export function short(a: string) {
  return `${a.slice(0, 6)}…${a.slice(-4)}`;
}
