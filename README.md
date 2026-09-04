# Pump.fun Telegram Trading Bot

A Telegram bot for trading pump.fun tokens — paste a contract to buy, track creator wallets, and auto-snipe launches with preset buy percentages.

## Features

- **Interactive Buy Flow** — Paste a mint address → see token info → choose % of balance → confirm buy
- **Interactive Sell Flow** — Paste a mint → see your balance → choose % to sell → confirm
- **Wallet Tracking** — Track creator wallets; auto-buy when they launch new tokens
- **Balance Check** — Check SOL balance and token balances
- **Position Tracking** — View all open positions with entry prices
- **Real-time Monitoring** — Polls pump.fun API every 5 seconds for new launches

## Architecture

```
pumpfun-telegram-bot/
├── main.py                 # Entry point
├── bot/
│   └── __init__.py         # Telegram bot with command handlers
├── pumpfun/
│   ├── __init__.py
│   └── client.py           # pump.fun trading client (buy/sell/balance)
├── data/                   # Runtime state (wallets, positions, trackers)
├── requirements.txt
├── .env.example
└── README.md
```

## Quick Start

1. **Install dependencies:**
   ```bash
   pip install -r requirements.txt
   ```

2. **Configure environment:**
   ```bash
   cp .env.example .env
   # Edit .env with your Telegram bot token and RPC URL
   ```

3. **Run:**
   ```bash
   python main.py
   ```

## Commands

| Command | Description |
|---------|-------------|
| `/start` | Welcome message and help |
| `/import <private_key>` | Import wallet (base58) |
| `/balance` | Check SOL balance |
| `/buy` | Interactive buy flow |
| `/sell` | Interactive sell flow |
| `/positions` | View open positions |
| `/track <wallet>` | Track a creator wallet |
| `/untrack <wallet>` | Stop tracking |
| `/tracks` | List tracked wallets |
| `/setpct <percentage>` | Set default buy % for auto-buys |

## How It Works

### Buy Flow
1. User sends `/buy`
2. Bot asks for mint address
3. User pastes contract address
4. Bot fetches token info (symbol, name, price, market cap)
5. Bot asks what % of balance to spend
6. User sends percentage (e.g. `10`)
7. Bot shows order summary with confirm/cancel buttons
8. On confirm, bot executes buy via pump.fun bonding curve

### Wallet Tracking (Auto-Buy)
1. User sends `/track <creator_wallet>`
2. Bot starts monitoring that wallet
3. When the wallet launches a new token, bot auto-buys using preset %
4. User gets notified with tx details

### Trading Client
The `PumpFunClient` class handles:
- `buy_token(mint, amount_sol)` — Buy on bonding curve via `buy_v2`
- `sell_token(mint, token_amount)` — Sell on bonding curve via `sell_v2`
- `get_balance()` — SOL balance
- `get_token_balance(mint)` — Token balance
- `get_token_info(mint)` — Token metadata from pump.fun API
- `get_new_tokens(limit)` — Latest tokens from pump.fun API

## Technical Details

- **Language:** Python 3.11+
- **Libraries:** python-telegram-bot, solders, solana, aiohttp
- **Instructions:** `buy_v2` (27 accounts), `sell_v2` (26 accounts) per official pump-public-docs
- **Slippage:** 50% (configurable)
- **Priority Fee:** 100,000 lamports / 200,000 CU (configurable)
- **Blockhash Cache:** 30s TTL

## Security Notes

- Private keys are stored locally in `data/wallets.json` (base58 encoded)
- Never share your private key
- The bot only trades when you explicitly confirm or when auto-buy triggers
- All transactions are on-chain and verifiable

## Disclaimer

This bot trades real assets on Solana. Use at your own risk. Always test with small amounts first.
