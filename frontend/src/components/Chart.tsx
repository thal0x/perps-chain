import { useEffect, useRef } from "react";
import {
  ColorType,
  createChart,
  type IChartApi,
  type ISeriesApi,
  type UTCTimestamp,
} from "lightweight-charts";
import { INTERVALS, type Interval } from "../lib/market";
import type { Candle } from "../lib/types";

interface Props {
  candles: Candle[];
  rev: number;
  interval: Interval;
  onInterval: (i: Interval) => void;
}

export function Chart({ candles, rev, interval, onInterval }: Props) {
  const el = useRef<HTMLDivElement>(null);
  const chart = useRef<IChartApi | null>(null);
  const cs = useRef<ISeriesApi<"Candlestick"> | null>(null);
  const vs = useRef<ISeriesApi<"Histogram"> | null>(null);
  const lastRev = useRef(-1);

  useEffect(() => {
    if (!el.current) return;
    const c = createChart(el.current, {
      autoSize: true,
      layout: { background: { type: ColorType.Solid, color: "transparent" }, textColor: "#8b98a5", fontSize: 11 },
      grid: { vertLines: { color: "#1b2530" }, horzLines: { color: "#1b2530" } },
      rightPriceScale: { borderColor: "#24303c" },
      timeScale: { borderColor: "#24303c", timeVisible: true, secondsVisible: false },
      crosshair: { mode: 0 },
    });
    cs.current = c.addCandlestickSeries({
      upColor: "#26c6a1",
      downColor: "#ef5f6c",
      borderVisible: false,
      wickUpColor: "#26c6a1",
      wickDownColor: "#ef5f6c",
    });
    vs.current = c.addHistogramSeries({ priceScaleId: "vol", priceFormat: { type: "volume" }, color: "#2a3a4a" });
    c.priceScale("vol").applyOptions({ scaleMargins: { top: 0.82, bottom: 0 } });
    chart.current = c;
    return () => {
      c.remove();
      chart.current = null;
    };
  }, []);

  useEffect(() => {
    if (!cs.current || !vs.current) return;
    const data = candles.map((k) => ({
      time: (k.t / 1000) as UTCTimestamp,
      open: +k.o,
      high: +k.h,
      low: +k.l,
      close: +k.c,
    }));
    const vol = candles.map((k) => ({
      time: (k.t / 1000) as UTCTimestamp,
      value: +k.v,
      color: +k.c >= +k.o ? "rgba(38,198,161,0.35)" : "rgba(239,95,108,0.35)",
    }));
    if (rev !== lastRev.current || data.length <= 1) {
      cs.current.setData(data);
      vs.current.setData(vol);
      if (rev !== lastRev.current) chart.current?.timeScale().fitContent();
      lastRev.current = rev;
    } else {
      cs.current.update(data[data.length - 1]);
      vs.current.update(vol[vol.length - 1]);
    }
  }, [candles, rev]);

  return (
    <div className="panel chart" data-testid="chart">
      <div className="panel-head">
        <div className="seg">
          {INTERVALS.map((i) => (
            <button key={i} className={i === interval ? "active" : ""} onClick={() => onInterval(i)}>
              {i}
            </button>
          ))}
        </div>
        <span className="dim small">candleSnapshot + candle WS</span>
      </div>
      <div className="chart-body">
        <div ref={el} className="chart-canvas" />
        {candles.length === 0 && <div className="chart-empty">No trades on this market yet — candles appear after the first fill.</div>}
      </div>
    </div>
  );
}
