"""
Entry point for the Telegram pump.fun trading bot.
"""

import logging
import asyncio
from typing import Optional
from telegram import Update, BotCommand
from telegram.ext import Application, CommandHandler, CallbackQueryHandler, MessageHandler, filters, ConversationHandler, ContextTypes
from dotenv import load_dotenv
import os

from pumpfun.client import PumpFunClient
from solders.keypair import Keypair
import base58

load_dotenv()

# ═══════════════════════════════════════════════════════════════
# Configuration
# ═══════════════════════════════════════════════════════════════

TELEGRAM_BOT_TOKEN = os.getenv("TELEGRAM_BOT_TOKEN")
RPC_URL = os.getenv("RPC_URL", "https://api.mainnet-beta.solana.com")
DATA_DIR = os.path.join(os.path.dirname(__file__), "data")
os.makedirs(DATA_DIR, exist_ok=True)

WALLETS_FILE = os.path.join(DATA_DIR, "wallets.json")
TRACKERS_FILE = os.path.join(DATA_DIR, "trackers.json")
POSITIONS_FILE = os.path.join(DATA_DIR, "positions.json")

logging.basicConfig(
    format="%(asctime)s - %(name)s - %(levelname)s - %(message)s",
    level=logging.INFO
)
logger = logging.getLogger(__name__)

# ═══════════════════════════════════════════════════════════════
# Data Persistence
# ═══════════════════════════════════════════════════════════════

import json
from datetime import datetime
from pathlib import Path

def load_json(path: str) -> dict:
    if os.path.exists(path):
        with open(path, 'r') as f:
            return json.load(f)
    return {}

def save_json(path: str, data: dict):
    with open(path, 'w') as f:
        json.dump(data, f, indent=2, default=str)

# ═══════════════════════════════════════════════════════════════
# State Machine for Buy Conversation
# ═══════════════════════════════════════════════════════════════

(
    WAITING_FOR_MINT,
    WAITING_FOR_PERCENTAGE,
    CONFIRM_BUY,
) = range(3)

# ═══════════════════════════════════════════════════════════════
# Bot Class
# ═══════════════════════════════════════════════════════════════

class PumpFunBot:
    def __init__(self):
        self.app: Optional[Application] = None
        self.clients: dict = {}  # user_id -> client
        self.tracker_tasks: dict = {}  # user_id -> tracker task

    def initialize(self):
        """Initialize the bot application."""
        self.app = (
            Application.builder()
            .token(TELEGRAM_BOT_TOKEN)
            .post_init(self._set_commands)
            .build()
        )
        
        # Register handlers
        self.app.add_handler(CommandHandler("start", self.cmd_start))
        self.app.add_handler(CommandHandler("help", self.cmd_help))
        self.app.add_handler(CommandHandler("balance", self.cmd_balance))
        self.app.add_handler(CommandHandler("import", self.cmd_import_wallet))
        self.app.add_handler(CommandHandler("positions", self.cmd_positions))
        self.app.add_handler(CommandHandler("track", self.cmd_track_wallet))
        self.app.add_handler(CommandHandler("untrack", self.cmd_untrack_wallet))
        self.app.add_handler(CommandHandler("tracks", self.cmd_list_tracks))
        self.app.add_handler(CommandHandler("setpct", self.cmd_set_percentage))
        
        # Buy conversation handler
        buy_conv = ConversationHandler(
            entry_points=[CommandHandler("buy", self.cmd_buy)],
            states={
                WAITING_FOR_MINT: [
                    MessageHandler(filters.TEXT & ~filters.COMMAND, self.buy_receive_mint)
                ],
                WAITING_FOR_PERCENTAGE: [
                    MessageHandler(filters.TEXT & ~filters.COMMAND, self.buy_receive_percentage)
                ],
                CONFIRM_BUY: [
                    CallbackQueryHandler(self.buy_confirm, pattern="^buy_confirm$"),
                    CallbackQueryHandler(self.buy_cancel, pattern="^buy_cancel$"),
                ],
            },
            fallbacks=[CommandHandler("cancel", self.buy_cancel_cmd)],
        )
        self.app.add_handler(buy_conv)
        
        # Sell conversation handler
        sell_conv = ConversationHandler(
            entry_points=[CommandHandler("sell", self.cmd_sell)],
            states={
                WAITING_FOR_MINT: [
                    MessageHandler(filters.TEXT & ~filters.COMMAND, self.sell_receive_mint)
                ],
                WAITING_FOR_PERCENTAGE: [
                    MessageHandler(filters.TEXT & ~filters.COMMAND, self.sell_receive_percentage)
                ],
            },
            fallbacks=[CommandHandler("cancel", self.buy_cancel_cmd)],
        )
        self.app.add_handler(sell_conv)
        
        # Callback handler for sell buttons
        self.app.add_handler(CallbackQueryHandler(self.sell_button_callback, pattern="^sell_"))

    async def _set_commands(self, application: Application):
        """Set the bot command menu in Telegram UI."""
        commands = [
            BotCommand("start", "🚀 Start bot & show help"),
            BotCommand("balance", "💰 Check wallet balance"),
            BotCommand("import", "📥 Import wallet (private key)"),
            BotCommand("buy", "🪙 Buy a token (interactive)"),
            BotCommand("sell", "💸 Sell a token (interactive)"),
            BotCommand("positions", "📊 View open positions"),
            BotCommand("track", "👁️ Track a creator wallet"),
            BotCommand("untrack", "🚫 Stop tracking a wallet"),
            BotCommand("tracks", "📋 List tracked wallets"),
            BotCommand("setpct", "📊 Set default buy %"),
            BotCommand("help", "❓ Show help"),
        ]
        await application.bot.set_my_commands(commands)
        logger.info("Bot command menu set")

    def get_client(self, user_id: int) -> Optional[PumpFunClient]:
        """Get or create a PumpFunClient for a user."""
        if user_id not in self.clients:
            wallets = load_json(WALLETS_FILE)
            user_wallets = wallets.get(str(user_id), {})
            if not user_wallets:
                return None
            pk_b58 = user_wallets.get("private_key", "")
            if not pk_b58:
                return None
            pk_bytes = base58.b58decode(pk_b58)
            keypair = Keypair.from_bytes(pk_bytes)
            self.clients[user_id] = PumpFunClient(RPC_URL, keypair)
        return self.clients[user_id]

    # ═══════════════════════════════════════════════════════════════
    # Command Handlers
    # ═══════════════════════════════════════════════════════════════

    async def cmd_start(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Start command — welcome message."""
        welcome = (
            "🚀 <b>Pump.fun Telegram Trading Bot</b>\n\n"
            "Welcome! I can help you trade pump.fun tokens directly from Telegram.\n\n"
            "<b>Quick Start:</b>\n"
            "1️⃣ Import your wallet with /import\n"
            "2️⃣ Buy tokens with /buy\n"
            "3️⃣ Track creator wallets with /track\n"
            "4️⃣ Set default buy percentage with /setpct\n\n"
            "<b>Commands:</b>\n"
            "/balance — Check wallet balance\n"
            "/positions — View open positions\n"
            "/buy — Buy a token (interactive)\n"
            "/sell — Sell a token (interactive)\n"
            "/track <code>&lt;wallet&gt;</code> — Track a creator wallet\n"
            "/untrack <code>&lt;wallet&gt;</code> — Stop tracking\n"
            "/tracks — List tracked wallets\n"
            "/setpct <code>&lt;percentage&gt;</code> — Set default buy %\n"
            "/help — Show this help\n\n"
            "⚠️ <b>Warning:</b> This bot trades real assets. Use at your own risk."
        )
        await update.message.reply_html(welcome)

    async def cmd_help(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Help command."""
        await self.cmd_start(update, context)

    async def cmd_balance(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Check wallet balance."""
        user_id = update.effective_user.id
        client = self.get_client(user_id)
        if not client:
            await update.message.reply_text(
                "❌ No wallet imported. Use /import first."
            )
            return
        
        try:
            balance = await client.get_balance()
            await update.message.reply_html(
                f"💰 <b>Wallet Balance</b>\n\n"
                f"<code>{client.keypair.pubkey()}</code>\n\n"
                f"<b>{balance:.4f} SOL</b>"
            )
        except Exception as e:
            await update.message.reply_text(f"❌ Error: {e}")

    async def cmd_import_wallet(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Import a wallet via private key."""
        if not context.args:
            await update.message.reply_html(
                "📥 <b>Import Wallet</b>\n\n"
                "Send your private key (base58 encoded) as a message.\n"
                "Example: <code>/import YourBase58PrivateKeyHere</code>\n\n"
                "⚠️ Your private key is stored locally and never shared."
            )
            return
        
        pk_b58 = context.args[0].strip()
        try:
            pk_bytes = base58.b58decode(pk_b58)
            keypair = Keypair.from_bytes(pk_bytes)
            
            wallets = load_json(WALLETS_FILE)
            wallets[str(update.effective_user.id)] = {
                "private_key": pk_b58,
                "pubkey": str(keypair.pubkey()),
                "imported_at": datetime.utcnow().isoformat(),
            }
            save_json(WALLETS_FILE, wallets)
            
            # Initialize client
            self.clients[update.effective_user.id] = PumpFunClient(RPC_URL, keypair)
            
            await update.message.reply_html(
                f"✅ <b>Wallet Imported</b>\n\n"
                f"<code>{keypair.pubkey()}</code>\n\n"
                f"Use /balance to check your balance."
            )
        except Exception as e:
            await update.message.reply_text(f"❌ Invalid private key: {e}")

    async def cmd_positions(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """View open positions."""
        positions = load_json(POSITIONS_FILE)
        user_positions = [
            p for p in positions.values() if p.get("user_id") == update.effective_user.id
        ]
        
        if not user_positions:
            await update.message.reply_text("📭 No open positions.")
            return
        
        text = "📊 <b>Your Positions</b>\n\n"
        for pos in user_positions:
            text += (
                f"🪙 <b>{pos.get('symbol', '???')}</b>\n"
                f"   Mint: <code>{pos.get('mint', '')[:20]}...</code>\n"
                f"   Amount: {pos.get('token_amount', 0):.0f} tokens\n"
                f"   Entry: {pos.get('entry_sol', 0):.4f} SOL\n"
                f"   Bought: {pos.get('bought_at', 'unknown')}\n\n"
            )
        
        await update.message.reply_html(text)

    # ═══════════════════════════════════════════════════════════════
    # Buy Flow (Interactive)
    # ═══════════════════════════════════════════════════════════════

    async def cmd_buy(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Start the buy flow."""
        user_id = update.effective_user.id
        client = self.get_client(user_id)
        if not client:
            await update.message.reply_text("❌ No wallet imported. Use /import first.")
            return ConversationHandler.END
        
        await update.message.reply_text(
            "🪙 <b>Buy Token</b>\n\n"
            "Send the token's <b>contract address (mint)</b>.\n"
            "Or send /cancel to abort.",
            parse_mode="HTML"
        )
        return WAITING_FOR_MINT

    async def buy_receive_mint(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Receive mint address from user."""
        mint = update.message.text.strip()
        
        # Validate mint
        try:
            from solders.pubkey import Pubkey
            Pubkey.from_string(mint)
        except:
            await update.message.reply_text("❌ Invalid mint address. Try again or /cancel.")
            return WAITING_FOR_MINT
        
        # Fetch token info
        client = self.get_client(update.effective_user.id)
        try:
            info = await client.get_token_info(mint)
            if info:
                context.user_data["buy_mint"] = mint
                context.user_data["buy_symbol"] = info.symbol
                context.user_data["buy_name"] = info.name
                await update.message.reply_html(
                    f"📋 <b>Token Info</b>\n\n"
                    f"Symbol: <b>{info.symbol}</b>\n"
                    f"Name: {info.name}\n"
                    f"Creator: <code>{info.creator[:20]}...</code>\n"
                    f"Price: {info.price_sol:.8f} SOL\n"
                    f"Market Cap: ${info.market_cap_usd:,.0f}\n"
                    f"Graduated: {'Yes' if info.complete else 'No'}\n\n"
                    f"What percentage of your balance to spend?\n"
                    f"Send a number (e.g. <code>10</code> for 10%)."
                )
                return WAITING_FOR_PERCENTAGE
            else:
                await update.message.reply_text("❌ Could not fetch token info. Check the mint address.")
                return WAITING_FOR_MINT
        except Exception as e:
            await update.message.reply_text(f"❌ Error: {e}")
            return WAITING_FOR_MINT

    async def buy_receive_percentage(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Receive buy percentage from user."""
        try:
            pct = float(update.message.text.strip().replace("%", ""))
            if pct <= 0 or pct > 100:
                raise ValueError
        except:
            await update.message.reply_text("❌ Invalid percentage. Send a number between 0.1 and 100.")
            return WAITING_FOR_PERCENTAGE
        
        user_id = update.effective_user.id
        client = self.get_client(user_id)
        
        try:
            balance = await client.get_balance()
            amount_sol = balance * (pct / 100)
            
            # Show confirmation
            symbol = context.user_data.get("buy_symbol", "???")
            mint = context.user_data["buy_mint"]
            
            keyboard = [
                [
                    InlineKeyboardButton("✅ Confirm Buy", callback_data="buy_confirm"),
                    InlineKeyboardButton("❌ Cancel", callback_data="buy_cancel"),
                ]
            ]
            
            await update.message.reply_html(
                f"📊 <b>Order Summary</b>\n\n"
                f"Token: <b>{symbol}</b>\n"
                f"Mint: <code>{mint[:20]}...</code>\n"
                f"Balance: {balance:.4f} SOL\n"
                f"Spend: <b>{pct}%</b> = <b>{amount_sol:.4f} SOL</b>\n\n"
                f"Ready to buy?",
                reply_markup=InlineKeyboardMarkup(keyboard)
            )
            
            context.user_data["buy_amount_sol"] = amount_sol
            return CONFIRM_BUY
        except Exception as e:
            await update.message.reply_text(f"❌ Error: {e}")
            return ConversationHandler.END

    async def buy_confirm(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Execute the buy."""
        query = update.callback_query
        await query.answer()
        
        user_id = update.effective_user.id
        client = self.get_client(user_id)
        
        mint = context.user_data["buy_mint"]
        amount_sol = context.user_data["buy_amount_sol"]
        symbol = context.user_data.get("buy_symbol", "???")
        
        await query.edit_message_text(
            f"⏳ Buying {symbol} with {amount_sol:.4f} SOL..."
        )
        
        try:
            tx = await client.buy_token(mint, amount_sol)
            
            # Save position
            positions = load_json(POSITIONS_FILE)
            pos_id = f"{user_id}_{mint}_{int(datetime.utcnow().timestamp())}"
            positions[pos_id] = {
                "user_id": user_id,
                "mint": mint,
                "symbol": symbol,
                "entry_sol": amount_sol,
                "token_amount": 0,  # Will be updated
                "bought_at": datetime.utcnow().isoformat(),
                "tx": tx,
            }
            save_json(POSITIONS_FILE, positions)
            
            await query.edit_message_text(
                f"✅ <b>Buy Successful!</b>\n\n"
                f"Token: <b>{symbol}</b>\n"
                f"Spent: {amount_sol:.4f} SOL\n"
                f"TX: <code>{tx}</code>\n\n"
                f"Use /positions to track your holdings.",
                parse_mode="HTML"
            )
        except Exception as e:
            await query.edit_message_text(f"❌ Buy failed: {e}")
        
        return ConversationHandler.END

    async def buy_cancel(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Cancel the buy."""
        query = update.callback_query
        await query.answer()
        await query.edit_message_text("❌ Buy cancelled.")
        return ConversationHandler.END

    async def buy_cancel_cmd(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Cancel command."""
        await update.message.reply_text("❌ Buy cancelled.")
        return ConversationHandler.END

    # ═══════════════════════════════════════════════════════════════
    # Sell Flow (Interactive)
    # ═══════════════════════════════════════════════════════════════

    async def cmd_sell(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Start the sell flow."""
        user_id = update.effective_user.id
        client = self.get_client(user_id)
        if not client:
            await update.message.reply_text("❌ No wallet imported. Use /import first.")
            return ConversationHandler.END
        
        await update.message.reply_text(
            "💸 <b>Sell Token</b>\n\n"
            "Send the token's <b>contract address (mint)</b>.\n"
            "Or send /cancel to abort.",
            parse_mode="HTML"
        )
        return WAITING_FOR_MINT

    async def sell_receive_mint(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Receive mint address for sell."""
        mint = update.message.text.strip()
        
        try:
            from solders.pubkey import Pubkey
            Pubkey.from_string(mint)
        except:
            await update.message.reply_text("❌ Invalid mint address. Try again or /cancel.")
            return WAITING_FOR_MINT
        
        user_id = update.effective_user.id
        client = self.get_client(user_id)
        
        try:
            token_balance = await client.get_token_balance(mint)
            if token_balance <= 0:
                await update.message.reply_text("❌ You don't hold this token.")
                return ConversationHandler.END
            
            context.user_data["sell_mint"] = mint
            
            await update.message.reply_html(
                f"💸 <b>Sell Token</b>\n\n"
                f"Your balance: <b>{token_balance:.0f} tokens</b>\n\n"
                f"What percentage to sell?\n"
                f"Send a number (e.g. <code>50</code> for 50%)."
            )
            return WAITING_FOR_PERCENTAGE
        except Exception as e:
            await update.message.reply_text(f"❌ Error: {e}")
            return ConversationHandler.END

    async def sell_receive_percentage(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Receive sell percentage and execute."""
        try:
            pct = float(update.message.text.strip().replace("%", ""))
            if pct <= 0 or pct > 100:
                raise ValueError
        except:
            await update.message.reply_text("❌ Invalid percentage. Send a number between 0.1 and 100.")
            return WAITING_FOR_PERCENTAGE
        
        user_id = update.effective_user.id
        client = self.get_client(user_id)
        mint = context.user_data["sell_mint"]
        
        try:
            token_balance = await client.get_token_balance(mint)
            sell_amount = token_balance * (pct / 100)
            
            await update.message.reply_text(
                f"⏳ Selling {sell_amount:.0f} tokens ({pct}%)..."
            )
            
            tx = await client.sell_token(mint, sell_amount)
            
            await update.message.reply_html(
                f"✅ <b>Sell Successful!</b>\n\n"
                f"Sold: {sell_amount:.0f} tokens\n"
                f"TX: <code>{tx}</code>",
                parse_mode="HTML"
            )
        except Exception as e:
            await update.message.reply_text(f"❌ Sell failed: {e}")
        
        return ConversationHandler.END

    async def sell_button_callback(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle sell button callbacks from position messages."""
        query = update.callback_query
        await query.answer()
        
        # Parse callback data: sell_<mint>_<percentage>
        parts = query.data.split("_")
        if len(parts) != 3:
            return
        
        _, mint, pct_str = parts
        try:
            pct = float(pct_str)
        except:
            return
        
        user_id = update.effective_user.id
        client = self.get_client(user_id)
        
        try:
            token_balance = await client.get_token_balance(mint)
            sell_amount = token_balance * (pct / 100)
            
            await query.edit_message_text(f"⏳ Selling {sell_amount:.0f} tokens...")
            
            tx = await client.sell_token(mint, sell_amount)
            
            await query.edit_message_text(
                f"✅ Sold {sell_amount:.0f} tokens ({pct}%)\n"
                f"TX: <code>{tx}</code>",
                parse_mode="HTML"
            )
        except Exception as e:
            await query.edit_message_text(f"❌ Sell failed: {e}")

    # ═══════════════════════════════════════════════════════════════
    # Wallet Tracking (Auto-Buy on Creator Launches)
    # ═══════════════════════════════════════════════════════════════

    async def cmd_track_wallet(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Track a creator wallet and auto-buy on launches."""
        if not context.args:
            await update.message.reply_html(
                "👁️ <b>Track Creator Wallet</b>\n\n"
                "Usage: <code>/track &lt;wallet_address&gt;</code>\n\n"
                "The bot will monitor this wallet and automatically buy "
                "when it launches a new token, using your preset percentage."
            )
            return
        
        wallet = context.args[0].strip()
        try:
            from solders.pubkey import Pubkey
            Pubkey.from_string(wallet)
        except:
            await update.message.reply_text("❌ Invalid wallet address.")
            return
        
        user_id = update.effective_user.id
        trackers = load_json(TRACKERS_FILE)
        
        if str(user_id) not in trackers:
            trackers[str(user_id)] = {}
        
        trackers[str(user_id)][wallet] = {
            "added_at": datetime.utcnow().isoformat(),
            "active": True,
        }
        save_json(TRACKERS_FILE, trackers)
        
        # Start tracker if not already running
        if user_id not in self.tracker_tasks or self.tracker_tasks[user_id].done():
            self.tracker_tasks[user_id] = asyncio.create_task(
                self.wallet_tracker(user_id)
            )
        
        await update.message.reply_html(
            f"✅ <b>Tracking Started</b>\n\n"
            f"Wallet: <code>{wallet[:20]}...</code>\n\n"
            f"When this wallet launches a new token, I'll automatically "
            f"buy using your preset percentage.\n\n"
            f"Use /setpct to set the buy percentage."
        )

    async def cmd_untrack_wallet(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Stop tracking a wallet."""
        if not context.args:
            await update.message.reply_text("Usage: /untrack <wallet_address>")
            return
        
        wallet = context.args[0].strip()
        user_id = update.effective_user.id
        trackers = load_json(TRACKERS_FILE)
        
        if str(user_id) in trackers and wallet in trackers[str(user_id)]:
            del trackers[str(user_id)][wallet]
            save_json(TRACKERS_FILE, trackers)
            await update.message.reply_text("✅ Stopped tracking this wallet.")
        else:
            await update.message.reply_text("❌ Wallet not found in tracking list.")

    async def cmd_list_tracks(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """List tracked wallets."""
        user_id = update.effective_user.id
        trackers = load_json(TRACKERS_FILE)
        user_tracks = trackers.get(str(user_id), {})
        
        if not user_tracks:
            await update.message.reply_text("📭 No wallets being tracked.")
            return
        
        text = "👁️ <b>Tracked Wallets</b>\n\n"
        for wallet, info in user_tracks.items():
            status = "🟢" if info.get("active") else "🔴"
            text += f"{status} <code>{wallet[:20]}...</code>\n"
        
        await update.message.reply_html(text)

    async def cmd_set_percentage(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Set default buy percentage for tracked wallet auto-buys."""
        if not context.args:
            await update.message.reply_html(
                "📊 <b>Set Buy Percentage</b>\n\n"
                "Usage: <code>/setpct &lt;percentage&gt;</code>\n"
                "Example: <code>/setpct 10</code> for 10%\n\n"
                "This percentage is used when auto-buying from tracked wallets."
            )
            return
        
        try:
            pct = float(context.args[0].strip().replace("%", ""))
            if pct <= 0 or pct > 100:
                raise ValueError
        except:
            await update.message.reply_text("❌ Invalid percentage.")
            return
        
        user_id = update.effective_user.id
        trackers = load_json(TRACKERS_FILE)
        
        if str(user_id) not in trackers:
            trackers[str(user_id)] = {}
        
        trackers[str(user_id)]["_default_pct"] = pct
        save_json(TRACKERS_FILE, trackers)
        
        await update.message.reply_html(
            f"✅ <b>Default buy percentage set to {pct}%</b>\n\n"
            f"When a tracked wallet launches a token, I'll spend {pct}% of your balance."
        )

    # ═══════════════════════════════════════════════════════════════
    # Wallet Tracker Loop
    # ═══════════════════════════════════════════════════════════════

    async def wallet_tracker(self, user_id: int):
        """Background task that monitors tracked wallets for new token launches."""
        logger.info(f"Starting wallet tracker for user {user_id}")
        
        seen_mints: set = set()
        
        while True:
            try:
                trackers = load_json(TRACKERS_FILE)
                user_tracks = trackers.get(str(user_id), {})
                
                # Get default buy percentage
                default_pct = user_tracks.get("_default_pct", 10)
                
                # Get active wallets
                active_wallets = [
                    w for w, info in user_tracks.items()
                    if w != "_default_pct" and info.get("active")
                ]
                
                if not active_wallets:
                    await asyncio.sleep(10)
                    continue
                
                client = self.get_client(user_id)
                if not client:
                    await asyncio.sleep(10)
                    continue
                
                # Fetch new tokens from pump.fun API
                new_tokens = await client.get_new_tokens(limit=50)
                
                for token_data in new_tokens:
                    mint = token_data.get("mint", "")
                    creator = token_data.get("creator", "")
                    
                    if mint in seen_mints:
                        continue
                    
                    # Check if creator is one of our tracked wallets
                    if creator in active_wallets:
                        seen_mints.add(mint)
                        
                        # Auto-buy!
                        try:
                            balance = await client.get_balance()
                            amount_sol = balance * (default_pct / 100)
                            
                            if amount_sol < 0.001:
                                logger.info(f"Balance too low for auto-buy: {balance}")
                                continue
                            
                            logger.info(
                                f"Auto-buying {token_data.get('symbol')} from tracked wallet {creator[:8]}"
                            )
                            
                            tx = await client.buy_token(mint, amount_sol)
                            
                            # Save position
                            positions = load_json(POSITIONS_FILE)
                            pos_id = f"{user_id}_{mint}_{int(datetime.utcnow().timestamp())}"
                            positions[pos_id] = {
                                "user_id": user_id,
                                "mint": mint,
                                "symbol": token_data.get("symbol", "???"),
                                "entry_sol": amount_sol,
                                "token_amount": 0,
                                "bought_at": datetime.utcnow().isoformat(),
                                "tx": tx,
                                "auto_buy": True,
                                "tracked_wallet": creator,
                            }
                            save_json(POSITIONS_FILE, positions)
                            
                            # Notify user
                            await self.app.bot.send_message(
                                chat_id=user_id,
                                text=(
                                    f"🚀 <b>Auto-Buy Triggered!</b>\n\n"
                                    f"Tracked wallet launched a new token!\n\n"
                                    f"Token: <b>{token_data.get('symbol', '???')}</b>\n"
                                    f"Mint: <code>{mint[:20]}...</code>\n"
                                    f"Spent: {amount_sol:.4f} SOL ({default_pct}%)\n"
                                    f"TX: <code>{tx}</code>"
                                ),
                                parse_mode="HTML"
                            )
                        except Exception as e:
                            logger.error(f"Auto-buy failed: {e}")
                
                # Poll every 5 seconds
                await asyncio.sleep(5)
                
            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"Tracker error: {e}")
                await asyncio.sleep(10)


# ═══════════════════════════════════════════════════════════════
# Entry Point
# ═══════════════════════════════════════════════════════════════

def main():
    bot = PumpFunBot()
    bot.initialize()
    logger.info("Bot started!")
    bot.app.run_polling(allowed_updates=Update.ALL_TYPES)

if __name__ == "__main__":
    main()
