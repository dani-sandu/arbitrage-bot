# Polymarket Arbitrage Bot

A high-performance arbitrage bot written in Rust that monitors Polymarket's 15-minute UP/DOWN crypto markets and automatically executes trades when profitable price discrepancies are detected.

## How It Works

Polymarket's 15-minute crypto markets offer paired binary options — an **UP token** and a **DOWN token**. At settlement, one token pays out $1.00 and the other $0.00. Because they are mutually exclusive, the fair sum of their prices is always **$1.00**.

The bot detects when:
```
UP_ask + DOWN_ask < ARBITRAGE_THRESHOLD (1.00 by default)
```
When this condition holds and the net spread exceeds fees, buying both sides locks in a risk-free profit at settlement.

---

## Features

### Arbitrage Detection
- Monitors live orderbook data via WebSocket for real-time price updates
- Checks profitability after taker fees before committing to any trade
- Enforces a minimum net spread of **0.5%** (`$0.005` per token pair) above fees
- Stale-price guard: re-validates prices immediately before order submission

### Trade Execution
- Executes UP and DOWN buy orders **in parallel** using `tokio::join!` to minimise leg risk
- FAK (Fill-And-Kill) limit orders via the Polymarket CLOB SDK
- Order sizes automatically capped to available liquidity on each side
- Token amount auto-capped to wallet balance at startup

### Partial Fill Protection
- If only one leg fills, the bot **automatically unwinds** the filled leg
- Unwind sell price is derived from the original buy price minus a configurable max slippage (`MAX_UNWIND_SLIPPAGE`, default 10%) — never a hardcoded fire-sale price
- Unwind failures trigger an immediate Telegram alert

### On-Chain Verification
- After every trade, queries CTF (Conditional Token Framework) token balances on Polygon via RPC
- Alerts if on-chain balance deviates from expected fill by more than 1 token

### Guard Chain
Multiple safety guards run in sequence before any trade is dispatched:

| # | Guard | Purpose |
|---|-------|---------|
| 1 | Profitability | Net spread must exceed fees + minimum threshold |
| 2 | Velocity lockout | Skip if combined ask_sum moved more than `VELOCITY_THRESHOLD` in the last 3 seconds |
| 3 | Spread guard | Skip if bid-ask spread on either side exceeds `MAX_SPREAD` (illiquid market) |
| 4 | Liquidity guard | Skip if available size on either side is below 5 tokens |
| 5 | Persistent state | Skip if this market has already been traded (survives restarts) |

### Persistent State & Reconciliation
- State saved to `state.json` in the data volume — survives container restarts
- Records cumulative PnL and total trade count across sessions
- Stores up to 50 detailed `TradeRecord` entries with order IDs, fill status, estimated PnL, and unwind flag for post-mortem analysis
- Partial fills are also marked as traded to prevent re-entry

### Telegram Notifications
- Bot startup and configuration summary
- Every completed arbitrage trade with token amounts and estimated profit
- Unwind events (both success and failure)
- On-chain position mismatches
- All alerts are fire-and-forget — they never block trade execution

### Approval Management
- Checks ERC-20 (USDC.e) allowance against a **100 USDC minimum** at startup
- Checks CTF `setApprovalForAll` for the Polymarket exchange contract
- Automatically submits approval transactions if needed, with on-chain confirmation

### Logging
- Structured logs to `monitor.log` and `error.log` in the data directory
- Log spam prevention: velocity lockout messages emit once per lockout period; market-closing warnings debounce to once per 10 seconds
- Coloured terminal output (cyan/green/red/yellow) for quick visual scanning

---

## Supported Assets

| Asset | Market Slug |
|-------|------------|
| BTC   | `btc-updown-15m` |
| ETH   | `eth-updown-15m` |
| SOL   | `sol-updown-15m` |
| XRP   | `xrp-updown-15m` |

---

## Requirements

- Docker & Docker Compose
- A Polymarket account with an API key and a funded Polygon wallet (USDC.e)
- _(Rust is **not** required locally — the Dockerfile handles compilation)_

---

## Setup

### 1. Clone the repository

```bash
git clone <repo-url>
cd polymarket-arbitrage-bot
```

### 2. Create your `.env` file

```bash
cp .env.example .env
```

Then edit `.env` with your values:

```env
# ── Wallet ──────────────────────────────────────────────────────────────────
PRIVATE_KEY=0xyour_private_key_here
PROXY_WALLET=0xyour_proxy_wallet_address
SIGNATURE_TYPE=EOA          # EOA or GNOSIS_SAFE

# ── Chain ───────────────────────────────────────────────────────────────────
RPC_URL=https://polygon-rpc.com

# ── Asset ───────────────────────────────────────────────────────────────────
# MARKET_ASSET is set per-container in docker-compose.yml.
# Only set this here if you are running a single container manually.
# MARKET_ASSET=BTC          # BTC | ETH | SOL | XRP

# ── Strategy ────────────────────────────────────────────────────────────────
TOKEN_AMOUNT=5.0            # Tokens to buy per side (min: 5.0)
ARBITRAGE_THRESHOLD=1.0     # Trigger when UP_ask + DOWN_ask < this
TAKER_FEE_RATE=0.02         # 2% taker fee
MAX_SPREAD=0.10             # Skip if bid-ask spread > 10%
VELOCITY_THRESHOLD=0.15     # Lockout if price moves > $0.15 in 3 seconds
VELOCITY_LOCKOUT_SECS=5     # Seconds to pause after a velocity event
MAX_UNWIND_SLIPPAGE=0.10    # Max 10% below buy price when unwinding a partial fill

# ── Telegram (optional) ─────────────────────────────────────────────────────
TELEGRAM_BOT_TOKEN=
TELEGRAM_CHAT_ID=

# ── Misc ────────────────────────────────────────────────────────────────────
DISPLAY_UI=false            # Set false when running in Docker
DATA_DIR=/app/data          # Persist logs and state via Docker volume

# ── SOCKS5 Proxy (optional) ─────────────────────────────────────────────────
# Routes CLOB order calls through a SOCKS5 proxy to bypass regional restrictions.
# Only affects order placement — WebSocket, Gamma API, and RPC calls are direct.
# Use socks5h:// (not socks5://) so DNS is resolved on the proxy side.
# SOCKS5_PROXY_URL=socks5h://user:pass@host:port
```

> **Security**: Never commit your `.env` file. It contains your private key.

---

## Running

### Start all markets (detached)

```bash
docker-compose up -d --build
```

### Start a single market

```bash
docker-compose up -d --build arb-btc
```

### View live logs

```bash
# All containers
docker-compose logs -f

# Specific market
docker-compose logs -f arb-eth
```

### Stop

```bash
# All markets
docker-compose down

# Single market
docker-compose stop arb-sol
```

### Rebuild after code changes

```bash
docker-compose build
docker-compose up -d
```

---

## Data & Logs

Each container writes to its own named Docker volume, mounted at `/app/data/<asset>`:

| Volume | Mount path | Container |
|--------|------------|-----------|
| `arb-btc-data` | `/app/data/btc` | `arb-btc` |
| `arb-eth-data` | `/app/data/eth` | `arb-eth` |
| `arb-sol-data` | `/app/data/sol` | `arb-sol` |
| `arb-xrp-data` | `/app/data/xrp` | `arb-xrp` |

Each volume contains:

| File | Contents |
|------|----------|
| `state.json` | Persistent trade state — traded markets, cumulative PnL, recent trade records |
| `monitor.log` | Rolling market monitor output |
| `error.log` | Error and warning entries |

To inspect a volume from the host:

```bash
docker-compose exec arb-btc cat /app/data/btc/state.json
docker-compose exec arb-eth tail -f /app/data/eth/monitor.log
```

---

## Configuration Reference

| Variable | Default | Description |
|----------|---------|-------------|
| `MARKET_ASSET` | `BTC` | Coin to monitor (`BTC`, `ETH`, `SOL`, `XRP`) |
| `TOKEN_AMOUNT` | `5.0` | Tokens to buy per side per trade |
| `ARBITRAGE_THRESHOLD` | `1.0` | Trigger threshold for UP + DOWN ask sum |
| `TAKER_FEE_RATE` | `0.02` | Taker fee rate used in profitability check |
| `MAX_SPREAD` | `0.10` | Max acceptable bid-ask spread per side |
| `VELOCITY_THRESHOLD` | `0.15` | Combined ask_sum movement to trigger lockout |
| `VELOCITY_LOCKOUT_SECS` | `5` | Seconds to pause after a velocity lockout |
| `MAX_UNWIND_SLIPPAGE` | `0.10` | Max slippage below buy price when unwinding |
| `SIGNATURE_TYPE` | `EOA` | Wallet type: `EOA` or `GNOSIS_SAFE` |
| `RPC_URL` | `https://polygon-rpc.com` | Polygon RPC endpoint |
| `CLOB_HTTP_URL` | `https://clob.polymarket.com` | Polymarket CLOB HTTP endpoint |
| `CLOB_WS_URL` | `wss://ws-subscriptions-clob.polymarket.com/ws/market` | Polymarket WebSocket endpoint |
| `DISPLAY_UI` | `false` | Terminal TUI mode (disable in Docker) |
| `DATA_DIR` | `./data` | Directory for logs and state file |
| `TELEGRAM_BOT_TOKEN` | _(empty)_ | Telegram bot token (optional) |
| `TELEGRAM_CHAT_ID` | _(empty)_ | Telegram chat ID (optional) |
| `SOCKS5_PROXY_URL` | _(empty)_ | SOCKS5 proxy for CLOB order calls (e.g. `socks5h://user:pass@host:port`) |
| `REDEEM_ENABLED` | `false` | Enable background redeemer on this instance (set on one container only) |
| `DRY_RUN` | `true` | Redeemer logs actions without sending transactions |
| `REDEEM_INTERVAL_SECS` | `300` | How often the redeemer sweeps for resolved positions (seconds) |

---

## Position Redeemer

The redeemer runs as a **background task inside the bot** — no separate container or cron job required. On the designated container it wakes up every `REDEEM_INTERVAL_SECS` seconds, sweeps all resolved positions, and redeems them on-chain. A Telegram alert is sent whenever positions are redeemed.

### How it works

1. **Fetch positions** — queries `data-api.polymarket.com/positions` for all token positions linked to your wallet
2. **Check on-chain balances** — queries the CTF (ERC-1155) contract on Polygon for the actual token balance of each position
3. **Check resolution** — queries the Gamma API to confirm which markets are resolved and whether they are NegRisk or standard CTF markets
4. **Simulate before sending** — runs `eth_call` simulation for every redemption before broadcasting, so no gas is wasted on a revert
5. **Redeem** — calls the correct contract:
   - Standard markets → `CTF.redeemPositions(collateral, parentCollectionId, conditionId, indexSets)`
   - NegRisk markets → `NegRiskAdapter.redeemPositions(conditionId, indexSets)`
   - Falls back automatically if the primary choice reverts in simulation

Both the winning and losing token for each market are redeemed in a single transaction. Only the winning side pays out $1.00 per token; the losing side returns $0.00.

### Enabling the redeemer

Only **one** container should run the redeemer to avoid redundant transactions. Enable it by setting `REDEEM_ENABLED=true` in the `environment:` block of the chosen service in `docker-compose.yml`:

```yaml
services:
  arb-btc:
    environment:
      - REDEEM_ENABLED=true   # ← only on this service
      - DRY_RUN=false         # ← send real transactions
```

All other services omit these lines (they default to `REDEEM_ENABLED=false`).

### Redeemer environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `REDEEM_ENABLED` | `false` | Enable the background redeemer on this instance |
| `DRY_RUN` | `true` | Log what would be redeemed without sending transactions |
| `REDEEM_INTERVAL_SECS` | `300` | Sweep interval in seconds |

### Contracts used

| Contract | Address | Purpose |
|----------|---------|---------|
| CTF (Conditional Tokens) | `0x4D97DCd97eC945f40cF65F87097ACe5EA0476045` | ERC-1155 token contract; direct redemption target for standard markets |
| NegRisk Adapter | `0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296` | Redemption target for NegRisk markets |
| USDC.e | `0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174` | Collateral token paid out on redemption |

---

## Architecture

```
src/
├── main.rs                  # Entry point, market loop, guard chain, background redeemer spawn
├── config/
│   ├── env.rs               # Environment variable loading with defaults
│   ├── constants.rs         # Available coins, API endpoints, order size limits
│   └── mod.rs
└── services/
│   ├── market_discovery.rs  # Gamma API — resolves active 15-min market IDs
│   ├── websocket_client.rs  # CLOB WebSocket — live orderbook subscription
│   ├── price_monitor.rs     # Price data aggregation and display
│   ├── arbitrage_executor.rs# Order execution, unwind logic, on-chain verification
│   ├── approvals.rs         # ERC-20 and CTF approval management
│   ├── chain_reader.rs      # On-chain balance queries (USDC.e, CTF tokens)
│   ├── redeemer.rs          # Background redeemer: fetch → balance check → simulate → redeem
│   ├── persistent_state.rs  # JSON state persistence and trade records
│   ├── velocity.rs          # Flash-move detection and lockout
│   └── create_clob_client.rs# Polymarket SDK wrapper (order signing, submission)
└── utils/
    ├── logger.rs            # Log file management
    ├── telegram.rs          # Telegram alert integration
    ├── keyboard.rs          # Terminal keyboard input
    └── coin_selector.rs     # Coin selection helpers
```

---

## Disclaimer

This bot trades real money on Polymarket. Use it at your own risk. Always start with a small `TOKEN_AMOUNT` (e.g. `5.0`) to validate behaviour before increasing position sizes. The authors are not responsible for any financial losses.
