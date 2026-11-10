import { useCallback, useEffect, useMemo, useState } from "react";
import { closeOtherClients, getClients, makeSigner } from "./lib/hl";
import { initialApiUrl, normalizeKey, saveApiUrl, saveKey, storedKey } from "./lib/settings";
import { useActiveCtx, useBook, useCandles, useMarkets, useTrades, useWsState, type Interval } from "./lib/market";
import { useAccount, useLeverage } from "./lib/account";
import { ToastProvider, Toasts } from "./lib/toasts";
import { MarketBar } from "./components/MarketBar";
import { OrderBook } from "./components/OrderBook";
import { Trades } from "./components/Trades";
import { Chart } from "./components/Chart";
import { OrderForm } from "./components/OrderForm";
import { AccountPanel, MarginSummary } from "./components/AccountPanel";
import { Settings, short } from "./components/Settings";
import { num } from "./lib/format";

export default function App() {
  return (
    <ToastProvider>
      <Terminal />
      <Toasts />
    </ToastProvider>
  );
}

function Terminal() {
  const [apiUrl, setApiUrl] = useState(initialApiUrl);
  const [key, setKey] = useState<`0x${string}` | null>(() => normalizeKey(storedKey() ?? ""));
  const [remember, setRemember] = useState(() => storedKey() !== null);
  const [showSettings, setShowSettings] = useState(false);
  const [coin, setCoin] = useState<string>(() => localStorage.getItem("hlbft.coin") ?? "BTC");
  const [interval, setIv] = useState<Interval>("1m");
  const [priceHint, setPriceHint] = useState<{ px: string; n: number } | null>(null);

  const clients = useMemo(() => getClients(apiUrl), [apiUrl]);
  useEffect(() => closeOtherClients(apiUrl), [apiUrl]);
  const signer = useMemo(() => (key ? makeSigner(clients.http, key) : null), [clients, key]);
  const user = signer?.address ?? null;

  const mk = useMarkets(clients);
  const market = mk.markets.find((m) => m.coin === coin) ?? mk.markets[0] ?? null;
  useEffect(() => {
    if (market && market.coin !== coin) setCoin(market.coin);
  }, [market, coin]);
  useEffect(() => {
    try {
      localStorage.setItem("hlbft.coin", coin);
    } catch {
      /* ignore */
    }
  }, [coin]);

  const liveCtx = useActiveCtx(clients, market);
  const ctx = liveCtx ?? (market ? mk.ctxs[market.coin] : null) ?? null;
  const book = useBook(clients, market?.coin ?? null);
  const trades = useTrades(clients, market?.coin ?? null);
  const candles = useCandles(clients, market?.coin ?? null, interval);
  const acct = useAccount(clients, user);
  const { lev, reload: reloadLev } = useLeverage(clients, user, market?.coin ?? null, market?.kind === "perp");
  const ws = useWsState(clients);

  const mid = market ? mk.mids[market.coin] ?? ctx?.midPx ?? undefined : undefined;
  const position =
    acct.ch?.assetPositions.map((a) => a.position).find((p) => p.coin === market?.coin && num(p.szi) !== 0) ?? null;

  const spotFree = (c: string) => {
    const b = acct.spot.find((x) => x.coin === c);
    return b ? String(num(b.total) - num(b.hold)) : undefined;
  };
  const avail =
    market?.kind === "spot"
      ? { buy: spotFree(market.quote), sell: spotFree(market.base) }
      : { buy: acct.ch?.withdrawable, sell: acct.ch?.withdrawable };

  const applySettings = useCallback((url: string, k: `0x${string}` | null, rem: boolean) => {
    setApiUrl(url);
    saveApiUrl(url);
    setKey(k);
    setRemember(rem);
    saveKey(rem ? k : null);
    setShowSettings(false);
  }, []);

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <span className="logo" />
          hlbft
        </div>
        <MarketBar markets={mk.markets} selected={market} ctx={ctx} mids={mk.mids} onSelect={setCoin} />
        <div className="grow" />
        <div className={`conn conn-${ws}`} title={`WebSocket ${ws}`} data-testid="ws-state">
          <span className="dot" /> {apiUrl.replace(/^https?:\/\//, "")}
        </div>
        <button className="btn" data-testid="open-settings" onClick={() => setShowSettings(true)}>
          {user ? <span className="mono" data-testid="user-address">{short(user)}</span> : "Connect key"}
        </button>
      </header>

      {mk.error && (
        <div className="banner err" data-testid="conn-error">
          {mk.error}. Is the localnet running (<code>scripts/localnet.sh dev</code>)? Change the URL in Settings.
        </div>
      )}

      <main className="grid">
        <Chart candles={candles.candles} rev={candles.rev} interval={interval} onInterval={setIv} />
        <div className="col-book">
          <OrderBook live={book} market={market} onPrice={(p) => setPriceHint({ px: p, n: Date.now() })} />
          <Trades trades={trades} />
        </div>
        <div className="col-trade">
          <OrderForm
            market={market}
            ctx={ctx}
            mid={mid}
            signer={signer}
            priceHint={priceHint}
            lev={lev}
            position={position}
            available={avail}
            onDone={() => void acct.refresh().catch(() => {})}
            onLevChanged={() => void reloadLev().catch(() => {})}
            onOpenSettings={() => setShowSettings(true)}
          />
          <MarginSummary acct={acct} />
        </div>
        <AccountPanel acct={acct} markets={mk.markets} ctxs={mk.ctxs} mids={mk.mids} signer={signer} onSelect={setCoin} />
      </main>

      {showSettings && (
        <Settings apiUrl={apiUrl} currentKey={key} remember={remember} onApply={applySettings} onClose={() => setShowSettings(false)} />
      )}
    </div>
  );
}
