import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";

/**
 * Dev-only helper: serves the localnet's funded keys so the UI can offer them
 * in a dropdown. Reads LOCALNET_ACCOUNTS, else ../run/localnet/accounts.json
 * (the file `scripts/localnet.sh dev` writes). Never part of `vite build`.
 */
function localnetAccounts(): Plugin {
  const file = resolve(process.env.LOCALNET_ACCOUNTS ?? resolve(__dirname, "../run/localnet/accounts.json"));
  return {
    name: "hlbft-localnet-accounts",
    apply: "serve",
    configureServer(server) {
      server.middlewares.use("/__localnet/accounts.json", async (_req, res) => {
        try {
          const body = await readFile(file, "utf8");
          JSON.parse(body);
          res.setHeader("content-type", "application/json");
          res.setHeader("cache-control", "no-store");
          res.end(body);
        } catch (e) {
          res.statusCode = 404;
          res.setHeader("content-type", "application/json");
          res.end(JSON.stringify({ error: `cannot read ${file}: ${(e as Error).message}` }));
        }
      });
    },
  };
}

export default defineConfig({
  plugins: [react(), localnetAccounts()],
  server: { port: Number(process.env.PORT ?? 5173), strictPort: false },
  preview: { port: Number(process.env.PORT ?? 4173) },
  build: { chunkSizeWarningLimit: 1500 },
});
