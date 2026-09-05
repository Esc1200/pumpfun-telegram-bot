"""
Entry point for the Telegram pump.fun trading bot.
"""

import logging
import asyncio
from typing import Optional
from telegram import Update, BotCommand, InlineKeyboardButton, InlineKeyboardMarkup
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
        self.app.add_handler(CommandHandler("setbuy", self.cmd_set_buy))
        self.app.add_handler(CommandHandler("alerts", self.cmd_alerts))
        self.app.add_handler(CallbackQueryHandler(self.setbuy_callback, pattern="^setbuy_"))
        
        # Buy/Sell/Track/Alert button callbacks
        self.app.add_handler(CallbackQueryHandler(self.buy_button_callback, pattern="^buy_"))
        self.app.add_handler(CallbackQueryHandler(self.sell_button_callback, pattern="^sell_"))
        self.app.add_handler(CallbackQueryHandler(self.track_creator_callback, pattern="^track_creator_"))
        self.app.add_handler(CallbackQueryHandler(self.track_trader_callback, pattern="^track_trader_"))
        self.app.add_handler(CallbackQueryHandler(self.fdv_alert_callback, pattern="^fdv_alert_"))
        self.app.add_handler(CallbackQueryHandler(self.fdv_set_callback, pattern="^fdv_set_"))
        self.app.add_handler(CallbackQueryHandler(self.fdv_custom_callback, pattern="^fdv_custom_"))
        self.app.add_handler(CallbackQueryHandler(self.fdv_buy_callback, pattern="^fdv_buy_"))
        self.app.add_handler(CallbackQueryHandler(self.fdv_sell_callback, pattern="^fdv_sell_"))
        self.app.add_handler(CallbackQueryHandler(self.fdv_sellcustom_callback, pattern="^fdv_sellcustom_"))
        self.app.add_handler(CallbackQueryHandler(self.alerts_callback, pattern="^at_"))
        
        # Paste address → detect token/wallet → show buttons
        self.app.add_handler(MessageHandler(
            filters.TEXT & ~filters.COMMAND & filters.Regex(r'^[1-9A-HJ-NP-Za-km-z]{32,44}$'),
            self.paste_contract_handler
        ))
        
        # Custom specify amount handler
        self.app.add_handler(MessageHandler(
            filters.TEXT & ~filters.COMMAND & filters.Regex(r'^\d*\.?\d+$'),
            self.specify_amount_handler
        ))

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
            BotCommand("setbuy", "📊 Set default buy amount"),
            BotCommand("alerts", "🔔 FDV Alerts"),
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
                    f"FDV: ${info.market_cap_usd:,.0f}\n"
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
            return
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
        
            tx = await client.sell_token(mint, sell_amount)
        
            # Edit back to grid with success status
            info = await client.get_token_info(mint)
            if info:
                sol_balance = await client.get_balance()
                token_balance = await client.get_token_balance(mint)
                keyboard = [
                    [InlineKeyboardButton("🟢 Buy 25%", callback_data=f"buy_{mint}_25"), InlineKeyboardButton("🔴 Sell 25%", callback_data=f"sell_{mint}_25")],
                    [InlineKeyboardButton("🟢 Buy 50%", callback_data=f"buy_{mint}_50"), InlineKeyboardButton("🔴 Sell 50%", callback_data=f"sell_{mint}_50")],
                    [InlineKeyboardButton("🟢 Buy 75%", callback_data=f"buy_{mint}_75"), InlineKeyboardButton("🔴 Sell 75%", callback_data=f"sell_{mint}_75")],
                    [InlineKeyboardButton("🟢 Buy 100%", callback_data=f"buy_{mint}_100"), InlineKeyboardButton("🔴 Sell 100%", callback_data=f"sell_{mint}_100")],
                    [InlineKeyboardButton("🔔 FDV Alert", callback_data=f"fdv_alert_{mint}")],
                ]
                text = f"🪙 <b>{info.symbol}</b> ({info.name})\n\n✅ <b>Sold {sell_amount:.0f} tokens ({pct}%)</b>\n\nTX: <code>{tx}</code>\n\nSelect action:"
                await query.edit_message_text(text, parse_mode="HTML", reply_markup=InlineKeyboardMarkup(keyboard))
                return
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
            if wallet.startswith("_"):
                continue  # skip internal keys like _default_pct
            status = "🟢" if isinstance(info, dict) and info.get("active") else "🔴"
            text += f"{status} <code>{wallet[:20]}...</code>\n"
        
        await update.message.reply_html(text)

    async def cmd_alerts(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Show FDV alerts with enable/disable options."""
        user_id = update.effective_user.id
        trackers = load_json(TRACKERS_FILE)
        user_tracks = trackers.get(str(user_id), {})
        
        # Get FDV alerts only
        fdv_alerts = {
            k: v for k, v in user_tracks.items()
            if k.startswith("fdv_alert_")
        }
        
        if not fdv_alerts:
            await update.message.reply_text("📭 No FDV alerts set.")
            return
        
        text = "🔔 <b>Your FDV Alerts</b>\n\n"
        keyboard = []
        
        for alert_key, alert in fdv_alerts.items():
            mint = alert.get("mint", "")
            target_fdv = alert.get("target_fdv", 0)
            active = alert.get("active", True)
            
            # Get token info for display
            symbol = "???"
            try:
                client = self.get_client(user_id)
                if client:
                    info = await client.get_token_info(mint)
                    if info:
                        symbol = info.symbol
            except:
                pass
            
            status = "🟢" if active else "🔴"
            direction = "Active" if active else "Disabled"
            
            text += (
                f"{status} <b>{symbol}</b>\n"
                f"   Target: ${target_fdv:,.0f}\n"
                f"   Mint: <code>{mint[:16]}...</code>\n"
                f"   Status: {direction}\n\n"
            )
            
            # Toggle button
            action = "Disable" if active else "Enable"
            emoji = "🔴" if active else "🟢"
            # Use shortened mint (last 8 chars) to fit 64-byte callback limit
            mint_short = mint[-8:] if len(mint) > 8 else mint
            keyboard.append([
                InlineKeyboardButton(
                    f"{emoji} {action} {symbol}",
                    callback_data=f"at_{mint_short}_{target_fdv}"
                )
            ])
        
        keyboard.append([
            InlineKeyboardButton("🗑️ Delete All", callback_data="at_delete_all")
        ])
        
        await update.message.reply_html(
            text,
            reply_markup=InlineKeyboardMarkup(keyboard)
        )

    async def setbuy_callback(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Show buttons for setting default buy amount (SOL or percentage)."""
        keyboard = [
            [InlineKeyboardButton("0.1 SOL", callback_data="setbuy_sol_0.1")],
            [InlineKeyboardButton("0.2 SOL", callback_data="setbuy_sol_0.2")],
            [InlineKeyboardButton("0.5 SOL", callback_data="setbuy_sol_0.5")],
            [InlineKeyboardButton("1 SOL", callback_data="setbuy_sol_1")],
            [InlineKeyboardButton("20%", callback_data="setbuy_pct_20")],
            [InlineKeyboardButton("50%", callback_data="setbuy_pct_50")],
            [InlineKeyboardButton("✏️ Specify SOL", callback_data="setbuy_specify_sol")],
            [InlineKeyboardButton("✏️ Specify %", callback_data="setbuy_specify_pct")],
        ]
        
        await update.message.reply_html(
            "📊 <b>Set Default Buy Amount</b>\n\n"
            "Choose how much to spend when auto-buying or copy-buying:\n\n"
            "<b>Fixed SOL:</b> Always spend this exact amount\n"
            "<b>Percentage:</b> Spend this % of your balance",
            reply_markup=InlineKeyboardMarkup(keyboard)
        )

    async def cmd_set_buy(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Redirect to setbuy_callback for button display."""
        await self.setbuy_callback(update, context)

    async def alerts_callback(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle alert toggle and delete buttons."""
        query = update.callback_query
        await query.answer()

        data = query.data.replace("at_", "")
        user_id = update.effective_user.id

        if data == "delete_all":
            trackers = load_json(TRACKERS_FILE)
            user_tracks = trackers.get(str(user_id), {})

            # Remove all FDV alerts
            to_remove = [k for k in user_tracks if k.startswith("fdv_alert_")]
            for k in to_remove:
                del user_tracks[k]

            trackers[str(user_id)] = user_tracks
            save_json(TRACKERS_FILE, trackers)

            await query.edit_message_text("✅ All FDV alerts deleted.")
            return

        # Toggle alert
        if data.startswith("toggle_"):
            parts = data.replace("toggle_", "").rsplit("_", 1)
            mint_short = parts[0]
            target_fdv = parts[1]

            trackers = load_json(TRACKERS_FILE)
            user_tracks = trackers.get(str(user_id), {})

            # Find the matching alert by comparing last 8 chars of mint AND target_fdv
            alert_key = None
            for k in user_tracks:
                if k.startswith("fdv_alert_") and k.endswith(mint_short) and k.endswith(f"_{target_fdv}"):
                    alert_key = k
                    break

            if alert_key:
                # Toggle active state
                current = user_tracks[alert_key].get("active", True)
                user_tracks[alert_key]["active"] = not current
                save_json(TRACKERS_FILE, trackers)

                new_status = "enabled" if user_tracks[alert_key]["active"] else "disabled"
                await query.edit_message_text(f"✅ Alert {new_status}.")
                return
            else:
                await query.edit_message_text("❌ Alert not found.")
                return

    async def setbuy_callback(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle setbuy button callbacks."""
        query = update.callback_query
        await query.answer()
        
        data = query.data.replace("setbuy_", "")
        user_id = update.effective_user.id
        
        if data.startswith("sol_"):
            amount = float(data.replace("sol_", ""))
            trackers = load_json(TRACKERS_FILE)
            if str(user_id) not in trackers:
                trackers[str(user_id)] = {}
            trackers[str(user_id)]["_default_sol"] = amount
            trackers[str(user_id)]["_default_pct"] = None  # Clear pct if SOL is set
            save_json(TRACKERS_FILE, trackers)
            
            await query.edit_message_text(
                f"✅ <b>Default buy set to {amount} SOL</b>\n\n"
                f"Auto-buys and copy-buys will spend exactly {amount} SOL.",
                parse_mode="HTML"
            )
        
        elif data.startswith("pct_"):
            pct = float(data.replace("pct_", ""))
            trackers = load_json(TRACKERS_FILE)
            if str(user_id) not in trackers:
                trackers[str(user_id)] = {}
            trackers[str(user_id)]["_default_pct"] = pct
            trackers[str(user_id)]["_default_sol"] = None  # Clear SOL if pct is set
            save_json(TRACKERS_FILE, trackers)
            
            await query.edit_message_text(
                f"✅ <b>Default buy set to {pct}%</b>\n\n"
                f"Auto-buys and copy-buys will spend {pct}% of your balance.",
                parse_mode="HTML"
            )
            return
        
        elif data == "specify_sol":
            context.user_data["awaiting_specify"] = "sol"
            await query.edit_message_text(
                "✏️ <b>Specify SOL Amount</b>\n\n"
                "Send a custom SOL amount (e.g. <code>0.3</code>)\n"
                "Minimum: 0.001 SOL",
                parse_mode="HTML"
            )
        
        elif data == "specify_pct":
            context.user_data["awaiting_specify"] = "pct"
            await query.edit_message_text(
                "✏️ <b>Specify Percentage</b>\n\n"
                "Send a custom percentage (e.g. <code>30</code>)\n"
                "Range: 1-100%",
                parse_mode="HTML"
            )

    # ═══════════════════════════════════════════════════════════════
    # Wallet Tracker Loop
    # ═══════════════════════════════════════════════════════════════

    async def fdv_set_callback(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle FDV target selection."""
        query = update.callback_query
        await query.answer()
        
        data = query.data.replace("fdv_set_", "")
        parts = data.rsplit("_", 1)
        mint = parts[0]
        target_fdv = float(parts[1])
        user_id = update.effective_user.id
        
        # Save FDV alert
        trackers = load_json(TRACKERS_FILE)
        if str(user_id) not in trackers:
            trackers[str(user_id)] = {}
        
        trackers[str(user_id)][f"fdv_alert_{mint}_{int(target_fdv)}"] = {
            "mint": mint,
            "target_fdv": target_fdv,
            "created_at": datetime.utcnow().isoformat(),
            "active": True,
        }
        save_json(TRACKERS_FILE, trackers)
        
        # Start FDV alert tracker if not already running
        fdv_key = f"fdv_{user_id}"
        if fdv_key not in self.tracker_tasks or self.tracker_tasks[fdv_key].done():
            self.tracker_tasks[fdv_key] = asyncio.create_task(
                self.fdv_alert_tracker(user_id)
            )
        
        await query.edit_message_text(
            f"✅ <b>FDV Alert Set!</b>\n\n"
            f"Target: <b>${target_fdv:,.0f}</b>\n\n"
            f"I'll notify you when the FDV crosses ${target_fdv:,.0f}.",
            parse_mode="HTML"
        )
        return

    async def fdv_custom_callback(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle custom FDV amount input."""
        query = update.callback_query
        await query.answer()
        
        mint = query.data.replace("fdv_custom_", "")
        user_id = update.effective_user.id
        
        context.user_data["awaiting_fdv"] = mint
        await query.edit_message_text(
            "✏️ <b>Custom FDV Target</b>\n\n"
            "Send a custom FDV amount (e.g. <code>250000</code> for $250K)\n"
            "Minimum: $1,000",
            parse_mode="HTML"
        )
        return

    async def fdv_buy_callback(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle FDV alert buy button - asks for SOL amount."""
        query = update.callback_query
        await query.answer()
        
        mint = query.data.replace("fdv_buy_", "")
        user_id = update.effective_user.id
        
        context.user_data["awaiting_fdv_buy"] = mint
        await query.edit_message_text(
            "🟢 <b>Buy with SOL</b>\n\n"
            "Send SOL amount to spend (e.g. <code>0.5</code>)\n"
            "Minimum: 0.001 SOL",
            parse_mode="HTML"
        )
        return

    async def fdv_sell_callback(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle FDV alert sell percentage buttons."""
        query = update.callback_query
        await query.answer()
        
        data = query.data.replace("fdv_sell_", "")
        parts = data.rsplit("_", 1)
        mint = parts[0]
        pct = float(parts[1])
        user_id = update.effective_user.id
        
        client = self.get_client(user_id)
        if not client:
            await query.edit_message_text("❌ No wallet imported.")
            return
        
        try:
            token_balance = await client.get_token_balance(mint)
            if token_balance <= 0:
                await query.edit_message_text("❌ You don't hold this token.")
                return
            
            sell_amount = token_balance * (pct / 100)
            await query.edit_message_text(f"⏳ Selling {sell_amount:.0f} tokens ({pct}%)...")
            
            tx = await client.sell_token(mint, sell_amount)
            
            # Edit back to grid
            info = await client.get_token_info(mint)
            if info:
                sol_balance = await client.get_balance()
                token_balance = await client.get_token_balance(mint)
                keyboard = [
                    [InlineKeyboardButton("🟢 Buy 25%", callback_data=f"buy_{mint}_25"), InlineKeyboardButton("🔴 Sell 25%", callback_data=f"sell_{mint}_25")],
                    [InlineKeyboardButton("🟢 Buy 50%", callback_data=f"buy_{mint}_50"), InlineKeyboardButton("🔴 Sell 50%", callback_data=f"sell_{mint}_50")],
                    [InlineKeyboardButton("🟢 Buy 75%", callback_data=f"buy_{mint}_75"), InlineKeyboardButton("🔴 Sell 75%", callback_data=f"sell_{mint}_75")],
                    [InlineKeyboardButton("🟢 Buy 100%", callback_data=f"buy_{mint}_100"), InlineKeyboardButton("🔴 Sell 100%", callback_data=f"sell_{mint}_100")],
                    [InlineKeyboardButton("🔔 FDV Alert", callback_data=f"fdv_alert_{mint}")],
                ]
                text = f"🪙 <b>{info.symbol}</b> ({info.name})\n\n✅ <b>Sold {sell_amount:.0f} tokens ({pct}%)</b>\n\nTX: <code>{tx}</code>\n\nSelect action:"
                await query.edit_message_text(text, parse_mode="HTML", reply_markup=InlineKeyboardMarkup(keyboard))
                return
        except Exception as e:
            await query.edit_message_text(f"❌ Sell failed: {e}")
        return

    async def fdv_sellcustom_callback(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle FDV alert custom sell button."""
        query = update.callback_query
        await query.answer()
        
        mint = query.data.replace("fdv_sellcustom_", "")
        # Handle fdv_sellcustom_{mint}_{amount} if present
        if "_" in mint:
            mint = mint.rsplit("_", 1)[0]
        user_id = update.effective_user.id
        
        context.user_data["awaiting_fdv_sell"] = mint
        await query.edit_message_text(
            "✏️ <b>Custom Sell Amount</b>\n\n"
            "Send token amount to sell (e.g. <code>5000</code>)",
            parse_mode="HTML"
        )
        return

    async def specify_amount_handler(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle custom SOL/percentage/FDV amount input."""
        user_id = update.effective_user.id
        text = update.message.text.strip()
        
        # Check for FDV input first
        if "awaiting_fdv" in context.user_data:
            mint = context.user_data.pop("awaiting_fdv")
            try:
                value = float(text)
            except:
                await update.message.reply_text("❌ Invalid number.")
                return
            
            if value < 1000:
                await update.message.reply_text("❌ Minimum FDV is $1,000.")
                return
            
            trackers = load_json(TRACKERS_FILE)
            if str(user_id) not in trackers:
                trackers[str(user_id)] = {}
            
            trackers[str(user_id)][f"fdv_alert_{mint}_{int(value)}"] = {
                "mint": mint,
                "target_fdv": value,
                "created_at": datetime.utcnow().isoformat(),
                "active": True,
            }
            save_json(TRACKERS_FILE, trackers)
        
        # Start FDV alert tracker if not already running
        fdv_key = f"fdv_{user_id}"
        if fdv_key not in self.tracker_tasks or self.tracker_tasks[fdv_key].done():
            self.tracker_tasks[fdv_key] = asyncio.create_task(
                self.fdv_alert_tracker(user_id)
            )
        
        # Edit back to the original grid
        client = self.get_client(user_id)
        if client:
            try:
                info = await client.get_token_info(mint)
                if info:
                    sol_balance = await client.get_balance()
                    token_balance = await client.get_token_balance(mint)
                    keyboard = [
                        [InlineKeyboardButton("🟢 Buy 25%", callback_data=f"buy_{mint}_25"), InlineKeyboardButton("🔴 Sell 25%", callback_data=f"sell_{mint}_25")],
                        [InlineKeyboardButton("🟢 Buy 50%", callback_data=f"buy_{mint}_50"), InlineKeyboardButton("🔴 Sell 50%", callback_data=f"sell_{mint}_50")],
                        [InlineKeyboardButton("🟢 Buy 75%", callback_data=f"buy_{mint}_75"), InlineKeyboardButton("🔴 Sell 75%", callback_data=f"sell_{mint}_75")],
                        [InlineKeyboardButton("🟢 Buy 100%", callback_data=f"buy_{mint}_100"), InlineKeyboardButton("🔴 Sell 100%", callback_data=f"sell_{mint}_100")],
                        [InlineKeyboardButton("🔔 FDV Alert", callback_data=f"fdv_alert_{mint}")],
                    ]
                    text = f"🪙 <b>{info.symbol}</b> ({info.name})\n\n💰 Your SOL: <b>{sol_balance:.4f}</b>\n🪙 Your tokens: <b>{token_balance:.0f}</b>\n💲 Price: {info.price_sol:.8f} SOL (${info.price_usd:.6f})\n📊 FDV: ${info.market_cap_usd:,.0f}\n\n✅ Alert set for ${value:,.0f}\nSelect action:"
                    await query.edit_message_text(text, parse_mode="HTML", reply_markup=InlineKeyboardMarkup(keyboard))
                    return
            except:
                pass
        
        await update.message.reply_html(
            f"✅ <b>FDV Alert Set!</b>\n\n"
            f"Target: <b>${value:,.0f}</b>\n\n"
            f"I'll notify you when the FDV crosses ${value:,.0f}."
        )
        return
        
        # Check for FDV buy input
        if "awaiting_fdv_buy" in context.user_data:
            mint = context.user_data.pop("awaiting_fdv_buy")
            try:
                amount_sol = float(text)
            except:
                await update.message.reply_text("❌ Invalid number.")
                return
            
            if amount_sol < 0.001:
                await update.message.reply_text("❌ Minimum is 0.001 SOL.")
                return
            
            client = self.get_client(user_id)
            if not client:
                await update.message.reply_text("❌ No wallet imported.")
                return
            
            try:
                tx = await client.buy_token(mint, amount_sol)
                
                # Edit back to grid
                info = await client.get_token_info(mint)
                if info:
                    sol_balance = await client.get_balance()
                    token_balance = await client.get_token_balance(mint)
                    keyboard = [
                        [InlineKeyboardButton("🟢 Buy 25%", callback_data=f"buy_{mint}_25"), InlineKeyboardButton("🔴 Sell 25%", callback_data=f"sell_{mint}_25")],
                        [InlineKeyboardButton("🟢 Buy 50%", callback_data=f"buy_{mint}_50"), InlineKeyboardButton("🔴 Sell 50%", callback_data=f"sell_{mint}_50")],
                        [InlineKeyboardButton("🟢 Buy 75%", callback_data=f"buy_{mint}_75"), InlineKeyboardButton("🔴 Sell 75%", callback_data=f"sell_{mint}_75")],
                        [InlineKeyboardButton("🟢 Buy 100%", callback_data=f"buy_{mint}_100"), InlineKeyboardButton("🔴 Sell 100%", callback_data=f"sell_{mint}_100")],
                        [InlineKeyboardButton("🔔 FDV Alert", callback_data=f"fdv_alert_{mint}")],
                    ]
                    text = f"🪙 <b>{info.symbol}</b> ({info.name})\n\n✅ <b>Buy Successful!</b>\n\nSpent: {amount_sol:.4f} SOL\nTX: <code>{tx}</code>\n\nSelect action:"
                    await query.edit_message_text(text, parse_mode="HTML", reply_markup=InlineKeyboardMarkup(keyboard))
                    return
            except Exception as e:
                await update.message.reply_text(f"❌ Buy failed: {e}")
            return
        
        # Check for FDV custom sell input
        if "awaiting_fdv_sell" in context.user_data:
            mint = context.user_data.pop("awaiting_fdv_sell")
            try:
                sell_amount = float(text)
            except:
                await update.message.reply_text("❌ Invalid number.")
                return
            
            if sell_amount <= 0:
                await update.message.reply_text("❌ Amount must be greater than 0.")
                return
            
            client = self.get_client(user_id)
            if not client:
                await update.message.reply_text("❌ No wallet imported.")
                return
            
            try:
                tx = await client.sell_token(mint, sell_amount)
                
                # Edit back to grid
                info = await client.get_token_info(mint)
                if info:
                    sol_balance = await client.get_balance()
                    token_balance = await client.get_token_balance(mint)
                    keyboard = [
                        [InlineKeyboardButton("🟢 Buy 25%", callback_data=f"buy_{mint}_25"), InlineKeyboardButton("🔴 Sell 25%", callback_data=f"sell_{mint}_25")],
                        [InlineKeyboardButton("🟢 Buy 50%", callback_data=f"buy_{mint}_50"), InlineKeyboardButton("🔴 Sell 50%", callback_data=f"sell_{mint}_50")],
                        [InlineKeyboardButton("🟢 Buy 75%", callback_data=f"buy_{mint}_75"), InlineKeyboardButton("🔴 Sell 75%", callback_data=f"sell_{mint}_75")],
                        [InlineKeyboardButton("🟢 Buy 100%", callback_data=f"buy_{mint}_100"), InlineKeyboardButton("🔴 Sell 100%", callback_data=f"sell_{mint}_100")],
                        [InlineKeyboardButton("🔔 FDV Alert", callback_data=f"fdv_alert_{mint}")],
                    ]
                    text = f"🪙 <b>{info.symbol}</b> ({info.name})\n\n✅ <b>Sold {sell_amount:.0f} tokens</b>\n\nTX: <code>{tx}</code>\n\nSelect action:"
                    await query.edit_message_text(text, parse_mode="HTML", reply_markup=InlineKeyboardMarkup(keyboard))
                    return
            except Exception as e:
                await update.message.reply_text(f"❌ Sell failed: {e}")
            return
        
        # Check for SOL/pct input
        if "awaiting_specify" not in context.user_data:
            return  # Not expecting input
        
        mode = context.user_data.pop("awaiting_specify")
        text = update.message.text.strip()
        user_id = update.effective_user.id
        
        try:
            value = float(text)
        except:
            await update.message.reply_text("❌ Invalid number.")
            return
        
        trackers = load_json(TRACKERS_FILE)
        if str(user_id) not in trackers:
            trackers[str(user_id)] = {}
        
        if mode == "sol":
            if value < 0.001:
                await update.message.reply_text("❌ Minimum is 0.001 SOL.")
                return
            trackers[str(user_id)]["_default_sol"] = value
            trackers[str(user_id)]["_default_pct"] = None
            save_json(TRACKERS_FILE, trackers)
            await update.message.reply_html(
                f"✅ <b>Default buy set to {value} SOL</b>"
            )
        elif mode == "pct":
            if value <= 0 or value > 100:
                await update.message.reply_text("❌ Range is 1-100%.")
                return
            trackers[str(user_id)]["_default_pct"] = value
            trackers[str(user_id)]["_default_sol"] = None
            save_json(TRACKERS_FILE, trackers)
            await update.message.reply_html(
                f"✅ <b>Default buy set to {value}%</b>"
            )

    async def wallet_tracker(self, user_id: int):
        """Background task that monitors tracked wallets for new token launches."""
        logger.info(f"Starting wallet tracker for user {user_id}")
        
        seen_mints: set = set()
        
        while True:
            try:
                trackers = load_json(TRACKERS_FILE)
                user_tracks = trackers.get(str(user_id), {})
                
                # Get default buy settings
                default_pct = user_tracks.get("_default_pct")
                default_sol = user_tracks.get("_default_sol", 0.01)
                
                # Get active creator-tracked wallets
                active_wallets = [
                    w for w, info in user_tracks.items()
                    if w != "_default_pct" and info.get("active") and info.get("type") != "trader"
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
                            
                            # Use fixed SOL amount if set, otherwise use percentage
                            if default_sol:
                                amount_sol = default_sol
                            elif default_pct:
                                amount_sol = balance * (default_pct / 100)
                            else:
                                amount_sol = balance * 0.1  # Default 10%
                            
                            if amount_sol < 0.001:
                                logger.info(f"Balance too low for auto-buy: {balance}")
                                continue
                            
                            if amount_sol > balance * 0.95:
                                amount_sol = balance * 0.95  # Keep some SOL for fees
                            
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
                
                # Poll every 3 seconds
                await asyncio.sleep(3)
                
            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"Tracker error: {e}")
                await asyncio.sleep(10)

    # ═══════════════════════════════════════════════════════════════
    # Trader Tracker (Copy-Buy on Wallet Purchases)
    # ═══════════════════════════════════════════════════════════════

    async def trader_tracker(self, user_id: int):
        """Background task that monitors a wallet for new token holdings and copy-buys.
        
        Strategy: Poll the wallet's token accounts via Solana RPC every 3s.
        When a new pump.fun token appears that wasn't there before → copy-buy.
        """
        logger.info(f"Starting trader tracker for user {user_id}")
        
        # Track which mints the tracked wallets already hold (to detect new buys)
        wallet_holdings: dict = {}  # wallet -> set of mint addresses
        
        while True:
            try:
                trackers = load_json(TRACKERS_FILE)
                user_tracks = trackers.get(str(user_id), {})
                
                # Get default buy settings
                default_pct = user_tracks.get("_default_pct")
                default_sol = user_tracks.get("_default_sol", 0.01)
                
                # Get active trader-tracked wallets
                trader_wallets = [
                    w for w, info in user_tracks.items()
                    if w != "_default_pct" and info.get("active") and info.get("type") == "trader"
                ]
                
                if not trader_wallets:
                    await asyncio.sleep(10)
                    continue
                
                client = self.get_client(user_id)
                if not client:
                    await asyncio.sleep(10)
                    continue
                
                # For each tracked wallet, check its token accounts
                for wallet in trader_wallets:
                    try:
                        # Get all token accounts for this wallet
                        session = await client._get_session()
                        payload = {
                            "jsonrpc": "2.0",
                            "id": 1,
                            "method": "getTokenAccountsByOwner",
                            "params": [
                                wallet,
                                {"programId": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"},
                                {"encoding": "jsonParsed", "minContextSlot": 0}
                            ]
                        }
                        async with session.post(client.rpc_url, json=payload) as resp:
                            result = await resp.json()
                        
                        if "error" in result:
                            logger.debug(f"RPC error for wallet {wallet[:8]}: {result['error']}")
                            continue
                        
                        # Extract mint addresses
                        current_mints = set()
                        for account in result.get("result", {}).get("value", []):
                            try:
                                mint = account["account"]["data"]["parsed"]["info"]["mint"]
                                current_mints.add(mint)
                            except (KeyError, TypeError):
                                continue
                        
                        # Initialize if first run
                        if wallet not in wallet_holdings:
                            wallet_holdings[wallet] = current_mints
                            continue
                        
                        # Find new mints (bought since last check)
                        previous_mints = wallet_holdings[wallet]
                        new_mints = current_mints - previous_mints
                        
                        # Update holdings
                        wallet_holdings[wallet] = current_mints
                        
                        # Copy-buy each new mint
                        for mint in new_mints:
                            try:
                                # Verify it's a pump.fun token by checking bonding curve
                                curve = await client.fetch_bonding_curve(mint)
                                if curve.complete:
                                    continue  # Skip graduated tokens
                                
                                balance = await client.get_balance()
                                
                                # Use fixed SOL amount if set, otherwise use percentage
                                if default_sol:
                                    amount_sol = default_sol
                                elif default_pct:
                                    amount_sol = balance * (default_pct / 100)
                                else:
                                    amount_sol = balance * 0.1  # Default 10%
                                
                                if amount_sol < 0.001:
                                    logger.info(f"Balance too low for copy-buy: {balance}")
                                    continue
                                
                                if amount_sol > balance * 0.95:
                                    amount_sol = balance * 0.95  # Keep some SOL for fees
                                
                                logger.info(
                                    f"Copy-buying {mint[:8]}... from trader {wallet[:8]}"
                                )
                                
                                tx = await client.buy_token(mint, amount_sol)
                                
                                # Save position
                                positions = load_json(POSITIONS_FILE)
                                pos_id = f"{user_id}_{mint}_{int(datetime.utcnow().timestamp())}"
                                positions[pos_id] = {
                                    "user_id": user_id,
                                    "mint": mint,
                                    "entry_sol": amount_sol,
                                    "token_amount": 0,
                                    "bought_at": datetime.utcnow().isoformat(),
                                    "tx": tx,
                                    "auto_buy": True,
                                    "copy_trade": True,
                                    "trader_wallet": wallet,
                                }
                                save_json(POSITIONS_FILE, positions)
                                
                                # Notify user
                                await self.app.bot.send_message(
                                    chat_id=user_id,
                                    text=(
                                        f"📋 <b>Copy-Buy Triggered!</b>\n\n"
                                        f"Tracked trader bought a new token!\n\n"
                                        f"Mint: <code>{mint[:20]}...</code>\n"
                                        f"Trader: <code>{wallet[:20]}...</code>\n"
                                        f"Spent: {amount_sol:.4f} SOL ({default_pct}%)\n"
                                        f"TX: <code>{tx}</code>"
                                    ),
                                    parse_mode="HTML"
                                )
                            except Exception as e:
                                logger.error(f"Copy-buy failed for {mint[:8]}: {e}")
                    
                    except Exception as e:
                        logger.error(f"Error checking wallet {wallet[:8]}: {e}")
                
                # Poll every 3 seconds
                await asyncio.sleep(3)
                
            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"Trader tracker error: {e}")
                await asyncio.sleep(10)

    # ═══════════════════════════════════════════════════════════════
    # FDV Alert Tracker
    # ═══════════════════════════════════════════════════════════════

    async def fdv_alert_tracker(self, user_id: int):
        """Background task that monitors FDV alerts and notifies when crossed."""
        logger.info(f"Starting FDV alert tracker for user {user_id}")
        
        # Track which alerts have been triggered and their initial state
        alerted_crossings: set = set()  # Tracks mint+target that have been notified
        previous_state: dict = {}  # mint+target -> "above" or "below"
        
        while True:
            try:
                trackers = load_json(TRACKERS_FILE)
                user_tracks = trackers.get(str(user_id), {})
                
                # Get active FDV alerts only
                fdv_alerts = {
                    k: v for k, v in user_tracks.items()
                    if k.startswith("fdv_alert_") and v.get("target_fdv") and v.get("active", True)
                }
                
                if not fdv_alerts:
                    await asyncio.sleep(5)
                    continue
                
                client = self.get_client(user_id)
                if not client:
                    await asyncio.sleep(5)
                    continue
                
                for alert_key, alert in fdv_alerts.items():
                    mint = alert.get("mint", "")
                    target_fdv = alert.get("target_fdv", 0)
                    
                    if not mint or target_fdv <= 0:
                        continue
                    
                    try:
                        info = await client.get_token_info(mint)
                        if not info:
                            continue
                        
                        current_fdv = info.market_cap_usd
                        alert_id = f"{mint}_{target_fdv}"
                        
                        # Determine current side and track state
                        # On first check, just record the state without
                        # triggering any notification (establish baseline).
                        # This means if FDV is already above target,
                        # it will take a DOWN cross followed by UP cross
                        # to trigger the alert. This is the expected
                        # bidirectional behavior: it won't fire unless
                        # there's an actual crossing event.
                        if alert_id not in previous_state:
                            previous_state[alert_id] = current_side
                            continue
                        
                        prev_side = previous_state[alert_id]
                        
                        # Check if crossed the target (either direction)
                        crossed = False
                        if prev_side == "below" and current_side == "above":
                            crossed = True  # Crossed UP through target
                        elif prev_side == "above" and current_side == "below":
                            crossed = True  # Crossed DOWN through target
                        
                        # Update state
                        previous_state[alert_id] = current_side
                        
                        # Only notify once per crossing
                        if crossed and alert_id not in alerted_crossings:
                            alerted_crossings.add(alert_id)
                            
                            # Build notification with Buy/Sell buttons
                            keyboard = [
                                [
                                    InlineKeyboardButton("🟢 Buy (SOL)", callback_data=f"fdv_buy_{mint}"),
                                    InlineKeyboardButton("🔴 Sell 25%", callback_data=f"fdv_sell_{mint}_25"),
                                ],
                                [
                                    InlineKeyboardButton("🔴 Sell 50%", callback_data=f"fdv_sell_{mint}_50"),
                                    InlineKeyboardButton("🔴 Sell 75%", callback_data=f"fdv_sell_{mint}_75"),
                                ],
                                [
                                    InlineKeyboardButton("🔴 Sell 100%", callback_data=f"fdv_sell_{mint}_100"),
                                    InlineKeyboardButton("🔴 Sell Custom", callback_data=f"fdv_sellcustom_{mint}"),
                                ],
                            ]
                            
                            # Determine direction text
                            direction = "📈 UP" if current_side == "above" else "📉 DOWN"
                            
                            await self.app.bot.send_message(
                                chat_id=user_id,
                                text=(
                                    f"🔔 <b>FDV Alert Triggered!</b> {direction}\n\n"
                                    f"Token: <b>{info.symbol}</b>\n"
                                    f"Target FDV: <b>${target_fdv:,.0f}</b>\n"
                                    f"Current FDV: <b>${current_fdv:,.0f}</b>\n"
                                    f"Mint: <code>{mint[:20]}...</code>\n\n"
                                    f"Quick trade:"
                                ),
                                parse_mode="HTML",
                                reply_markup=InlineKeyboardMarkup(keyboard)
                            )
                    except Exception as e:
                        logger.debug(f"FDV check failed for {mint}: {e}")
                
                # Poll every 5 seconds
                await asyncio.sleep(5)
                
            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"FDV alert tracker error: {e}")
                await asyncio.sleep(30)

    # ═══════════════════════════════════════════════════════════════
    # Paste Contract → Buy/Sell Grid
    # ═══════════════════════════════════════════════════════════════

    async def paste_contract_handler(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """When user pastes an address, detect if token or wallet and show appropriate buttons."""
        text = update.message.text.strip()
        
        # Validate it's a Solana pubkey
        try:
            from solders.pubkey import Pubkey
            Pubkey.from_string(text)
        except:
            return  # Not a valid address, ignore
        
        user_id = update.effective_user.id
        client = self.get_client(user_id)
        
        if not client:
            await update.message.reply_text("❌ No wallet imported. Use /import first.")
            return
        
        # Try to fetch token info
        try:
            info = await client.get_token_info(text)
        except:
            info = None
        
        if info:
            # It's a token → show buy/sell grid
            await self._show_token_grid(update, client, info, text)
        else:
            # It's a wallet → show tracking options
            await self._show_wallet_options(update, text)

    async def _show_token_grid(self, update, client, info, mint):
        """Show buy/sell grid for a token."""
        sol_balance = await client.get_balance()
        token_balance = await client.get_token_balance(mint)
        
        # 2x5 grid: Buy, Sell, and FDV Alert
        keyboard = [
            [
                InlineKeyboardButton("🟢 Buy 25%", callback_data=f"buy_{mint}_25"),
                InlineKeyboardButton("🔴 Sell 25%", callback_data=f"sell_{mint}_25"),
            ],
            [
                InlineKeyboardButton("🟢 Buy 50%", callback_data=f"buy_{mint}_50"),
                InlineKeyboardButton("🔴 Sell 50%", callback_data=f"sell_{mint}_50"),
            ],
            [
                InlineKeyboardButton("🟢 Buy 75%", callback_data=f"buy_{mint}_75"),
                InlineKeyboardButton("🔴 Sell 75%", callback_data=f"sell_{mint}_75"),
            ],
            [
                InlineKeyboardButton("🟢 Buy 100%", callback_data=f"buy_{mint}_100"),
                InlineKeyboardButton("🔴 Sell 100%", callback_data=f"sell_{mint}_100"),
            ],
            [
                InlineKeyboardButton("🔔 FDV Alert", callback_data=f"fdv_alert_{mint}"),
            ],
        ]
        
        # Show FDV alerts for this token
        trackers = load_json(TRACKERS_FILE)
        user_tracks = trackers.get(str(user_id), {})
        token_alerts = {}
        for k, v in user_tracks.items():
            if k.startswith("fdv_alert_") and v.get("mint") == mint and v.get("active", True):
                target = v.get("target_fdv", 0)
                if target > 0:
                    token_alerts[target] = k
        
        if token_alerts:
            alert_buttons = []
            for target in sorted(token_alerts.keys()):
                alert_buttons.append(
                    InlineKeyboardButton(
                        f"🔔 ${target:,.0f}",
                        callback_data=f"fdv_alert_{mint}_{int(target)}"
                    )
                )
            keyboard.append(alert_buttons)
        
        # Also add set alert button
        
        text = (
            f"🪙 <b>{info.symbol}</b> ({info.name})\n\n"
            f"💰 Your SOL: <b>{sol_balance:.4f}</b>\n"
            f"🪙 Your tokens: <b>{token_balance:.0f}</b>\n"
            f"💲 Price: {info.price_sol:.8f} SOL (${info.price_usd:.6f})\n"
            f"📊 FDV: ${info.market_cap_usd:,.0f}\n\n"
            f"Select action:"
        )
        
        if update.callback_query:
            # Edit existing message
            await update.callback_query.edit_message_text(
                text,
                parse_mode="HTML",
                reply_markup=InlineKeyboardMarkup(keyboard)
            )
        else:
            # Send new message
            msg = await update.message.reply_html(
                text,
                reply_markup=InlineKeyboardMarkup(keyboard)
            )
            # Store message_id for later editing
            user_id = update.effective_user.id
            context = update.effective_chat
            # Save to context for later use
            if not hasattr(self, '_last_grid_message'):
                self._last_grid_message = {}
            self._last_grid_message[user_id] = msg.message_id

    async def _edit_back_to_grid(self, query, client, mint):
        """Edit message back to the original token grid with existing alert buttons."""
        try:
            info = await client.get_token_info(mint)
            if not info:
                return
            
            sol_balance = await client.get_balance()
            token_balance = await client.get_token_balance(mint)
            
            keyboard = [
                [
                    InlineKeyboardButton("🟢 Buy 25%", callback_data=f"buy_{mint}_25"),
                    InlineKeyboardButton("🔴 Sell 25%", callback_data=f"sell_{mint}_25"),
                ],
                [
                    InlineKeyboardButton("🟢 Buy 50%", callback_data=f"buy_{mint}_50"),
                    InlineKeyboardButton("🔴 Sell 50%", callback_data=f"sell_{mint}_50"),
                ],
                [
                    InlineKeyboardButton("🟢 Buy 75%", callback_data=f"buy_{mint}_75"),
                    InlineKeyboardButton("🔴 Sell 75%", callback_data=f"sell_{mint}_75"),
                ],
                [
                    InlineKeyboardButton("🟢 Buy 100%", callback_data=f"buy_{mint}_100"),
                    InlineKeyboardButton("🔴 Sell 100%", callback_data=f"sell_{mint}_100"),
                ],
            ]
            
            # Load existing alerts for this token
            trackers = load_json(TRACKERS_FILE)
            user_tracks = trackers.get(str(query.from_user.id), {})
            alert_buttons = []
            for k, v in user_tracks.items():
                if k.startswith("fdv_alert_") and v.get("mint") == mint and v.get("active", True):
                    target = v.get("target_fdv", 0)
                    if target > 0:
                        alert_buttons.append(
                            InlineKeyboardButton(
                                f"🔔 ${target:,.0f}",
                                callback_data=f"fdv_alert_{mint}_{int(target)}"
                            )
                        )
            if alert_buttons:
                keyboard.append(alert_buttons)
            
            keyboard.append([InlineKeyboardButton("🔔 FDV Alert", callback_data=f"fdv_alert_{mint}")])
            
            text = (
                f"🪙 <b>{info.symbol}</b> ({info.name})\n\n"
                f"💰 Your SOL: <b>{sol_balance:.4f}</b>\n"
                f"🪙 Your tokens: <b>{token_balance:.0f}</b>\n"
                f"💲 Price: {info.price_sol:.8f} SOL (${info.price_usd:.6f})\n"
                f"📊 FDV: ${info.market_cap_usd:,.0f}\n\n"
                f"Select action:"
            )
            
            await query.edit_message_text(
                text,
                parse_mode="HTML",
                reply_markup=InlineKeyboardMarkup(keyboard)
            )
        except Exception as e:
            logger.error(f"Error editing back to grid: {e}")
        return

    async def _show_token_grid_with_alerts(self, update, client, info, mint):
        """Show buy/sell grid for a token with existing FDV alert buttons."""
        sol_balance = await client.get_balance()
        token_balance = await client.get_token_balance(mint)
        
        # 2x5 grid: Buy, Sell, and FDV Alert
        keyboard = [
            [
                InlineKeyboardButton("🟢 Buy 25%", callback_data=f"buy_{mint}_25"),
                InlineKeyboardButton("🔴 Sell 25%", callback_data=f"sell_{mint}_25"),
            ],
            [
                InlineKeyboardButton("🟢 Buy 50%", callback_data=f"buy_{mint}_50"),
                InlineKeyboardButton("🔴 Sell 50%", callback_data=f"sell_{mint}_50"),
            ],
            [
                InlineKeyboardButton("🟢 Buy 75%", callback_data=f"buy_{mint}_75"),
                InlineKeyboardButton("🔴 Sell 75%", callback_data=f"sell_{mint}_75"),
            ],
            [
                InlineKeyboardButton("🟢 Buy 100%", callback_data=f"buy_{mint}_100"),
                InlineKeyboardButton("🔴 Sell 100%", callback_data=f"sell_{mint}_100"),
            ],
            [
                InlineKeyboardButton("🔔 FDV Alert", callback_data=f"fdv_alert_{mint}"),
            ],
        ]
        
        # Show FDV alerts for this token
        trackers = load_json(TRACKERS_FILE)
        user_tracks = trackers.get(str(update.effective_user.id), {})
        token_alerts = {}
        for k, v in user_tracks.items():
            if k.startswith("fdv_alert_") and v.get("mint") == mint and v.get("active", True):
                target = v.get("target_fdv", 0)
                if target > 0:
                    token_alerts[target] = k
        
        if token_alerts:
            alert_buttons = []
            for target in sorted(token_alerts.keys()):
                alert_buttons.append(
                    InlineKeyboardButton(
                        f"🔔 ${target:,.0f}",
                        callback_data=f"fdv_alert_{mint}_{int(target)}"
                    )
                )
            keyboard.append(alert_buttons)
        
        text = (
            f"🪙 <b>{info.symbol}</b> ({info.name})\n\n"
            f"💰 Your SOL: <b>{sol_balance:.4f}</b>\n"
            f"🪙 Your tokens: <b>{token_balance:.0f}</b>\n"
            f"💲 Price: {info.price_sol:.8f} SOL (${info.price_usd:.6f})\n"
            f"📊 FDV: ${info.market_cap_usd:,.0f}\n\n"
            f"Select action:"
        )
        
        if update.callback_query:
            await update.callback_query.edit_message_text(
                text,
                parse_mode="HTML",
                reply_markup=InlineKeyboardMarkup(keyboard)
            )
        else:
            await update.message.reply_html(
                text,
                reply_markup=InlineKeyboardMarkup(keyboard)
            )

    async def _show_wallet_options(self, update, wallet):
        """Show tracking options for a wallet address."""
        keyboard = [
            [InlineKeyboardButton("👁️ Creator Track", callback_data=f"track_creator_{wallet}")],
            [InlineKeyboardButton("📋 Trader Track", callback_data=f"track_trader_{wallet}")],
        ]
        
        await update.message.reply_html(
            f"👛 <b>Wallet Detected</b>\n\n"
            f"<code>{wallet[:20]}...</code>\n\n"
            f"This is a wallet address, not a token.\n"
            f"Choose tracking mode:\n\n"
            f"<b>Creator Track:</b> Auto-buy when this wallet launches a new token\n"
            f"<b>Trader Track:</b> Copy-buy when this wallet buys a token",
            reply_markup=InlineKeyboardMarkup(keyboard)
        )

    async def track_creator_callback(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle creator track button."""
        query = update.callback_query
        await query.answer()
        
        wallet = query.data.replace("track_creator_", "")
        user_id = update.effective_user.id
        
        trackers = load_json(TRACKERS_FILE)
        if str(user_id) not in trackers:
            trackers[str(user_id)] = {}
        
        trackers[str(user_id)][wallet] = {
            "added_at": datetime.utcnow().isoformat(),
            "active": True,
            "type": "creator",
        }
        save_json(TRACKERS_FILE, trackers)
        
        # Start tracker if not already running
        if user_id not in self.tracker_tasks or self.tracker_tasks[user_id].done():
            self.tracker_tasks[user_id] = asyncio.create_task(
                self.wallet_tracker(user_id)
            )
        
        await query.edit_message_text(
            f"✅ <b>Creator Tracking Started</b>\n\n"
            f"Wallet: <code>{wallet[:20]}...</code>\n\n"
            f"Auto-buy when this wallet launches a new token.\n"
            f"Use /setpct to set buy percentage.",
            parse_mode="HTML"
        )

    async def track_trader_callback(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle trader track button (copy-buy)."""
        query = update.callback_query
        await query.answer()
        
        wallet = query.data.replace("track_trader_", "")
        user_id = update.effective_user.id
        
        trackers = load_json(TRACKERS_FILE)
        if str(user_id) not in trackers:
            trackers[str(user_id)] = {}
        
        trackers[str(user_id)][wallet] = {
            "added_at": datetime.utcnow().isoformat(),
            "active": True,
            "type": "trader",
        }
        save_json(TRACKERS_FILE, trackers)
        
        # Start trader tracker if not already running
        trader_key = f"trader_{user_id}"
        if trader_key not in self.tracker_tasks or self.tracker_tasks[trader_key].done():
            self.tracker_tasks[trader_key] = asyncio.create_task(
                self.trader_tracker(user_id)
            )
        
        await query.edit_message_text(
            f"✅ <b>Trader Tracking Started</b>\n\n"
            f"Wallet: <code>{wallet[:20]}...</code>\n\n"
            f"I'll copy-buy whenever this wallet buys a token on pump.fun.\n"
            f"Use /setpct to set buy percentage.",
            parse_mode="HTML"
        )
        return

    async def buy_button_callback(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle buy percentage buttons (25/50/75/100%)."""
        query = update.callback_query
        await query.answer()
        
        # Parse callback data: buy_<mint>_<percentage>
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
        
        if not client:
            await query.edit_message_text("❌ No wallet imported. Use /import first.")
            return
        
        try:
            balance = await client.get_balance()
            amount_sol = balance * (pct / 100)
            
            if amount_sol < 0.001:
                await query.edit_message_text("❌ Balance too low. Need at least 0.001 SOL.")
                return
            
            tx = await client.buy_token(mint, amount_sol)
            
            # Save position
            positions = load_json(POSITIONS_FILE)
            pos_id = f"{user_id}_{mint}_{int(datetime.utcnow().timestamp())}"
            positions[pos_id] = {
                "user_id": user_id,
                "mint": mint,
                "entry_sol": amount_sol,
                "token_amount": 0,
                "bought_at": datetime.utcnow().isoformat(),
                "tx": tx,
            }
            save_json(POSITIONS_FILE, positions)
            
            # Edit back to grid with success status
            info = await client.get_token_info(mint)
            if info:
                sol_balance = await client.get_balance()
                token_balance = await client.get_token_balance(mint)
                keyboard = [
                    [InlineKeyboardButton("🟢 Buy 25%", callback_data=f"buy_{mint}_25"), InlineKeyboardButton("🔴 Sell 25%", callback_data=f"sell_{mint}_25")],
                    [InlineKeyboardButton("🟢 Buy 50%", callback_data=f"buy_{mint}_50"), InlineKeyboardButton("🔴 Sell 50%", callback_data=f"sell_{mint}_50")],
                    [InlineKeyboardButton("🟢 Buy 75%", callback_data=f"buy_{mint}_75"), InlineKeyboardButton("🔴 Sell 75%", callback_data=f"sell_{mint}_75")],
                    [InlineKeyboardButton("🟢 Buy 100%", callback_data=f"buy_{mint}_100"), InlineKeyboardButton("🔴 Sell 100%", callback_data=f"sell_{mint}_100")],
                    [InlineKeyboardButton("🔔 FDV Alert", callback_data=f"fdv_alert_{mint}")],
                ]
                text = f"🪙 <b>{info.symbol}</b> ({info.name})\n\n✅ <b>Buy Successful!</b>\n\nSpent: {amount_sol:.4f} SOL ({pct}%)\nTX: <code>{tx}</code>\n\nUse /positions to track.\nSelect action:"
                await query.edit_message_text(text, parse_mode="HTML", reply_markup=InlineKeyboardMarkup(keyboard))
                return
        except Exception as e:
            await query.edit_message_text(f"❌ Buy failed: {e}")

    async def fdv_alert_callback(self, update: Update, context: ContextTypes.DEFAULT_TYPE):
        """Handle FDV alert button."""
        query = update.callback_query
        await query.answer()
        
        # Extract mint and optional target from callback data
        # Format: fdv_alert_{mint} or fdv_alert_{mint}_{target}
        parts = query.data.replace("fdv_alert_", "").rsplit("_", 1)
        mint = parts[0]
        user_id = update.effective_user.id
        
        # Show FDV alert options
        keyboard = [
            [
                InlineKeyboardButton("$100K", callback_data=f"fdv_set_{mint}_100000"),
                InlineKeyboardButton("$500K", callback_data=f"fdv_set_{mint}_500000"),
            ],
            [
                InlineKeyboardButton("$1M", callback_data=f"fdv_set_{mint}_1000000"),
                InlineKeyboardButton("$5M", callback_data=f"fdv_set_{mint}_5000000"),
            ],
            [
                InlineKeyboardButton("$10M", callback_data=f"fdv_set_{mint}_10000000"),
                InlineKeyboardButton("$50M", callback_data=f"fdv_set_{mint}_50000000"),
            ],
            [
                InlineKeyboardButton("$100M", callback_data=f"fdv_set_{mint}_100000000"),
                InlineKeyboardButton("✏️ Custom", callback_data=f"fdv_custom_{mint}"),
            ],
        ]
        
        # Get current FDV
        client = self.get_client(user_id)
        try:
            info = await client.get_token_info(mint)
            current_fdv = info.market_cap_usd if info else 0
        except:
            current_fdv = 0
        
        await query.edit_message_text(
            f"🔔 <b>FDV Alert</b>\n\n"
            f"Current FDV: <b>${current_fdv:,.0f}</b>\n\n"
            f"Select target FDV to be notified when crossed:",
            parse_mode="HTML",
            reply_markup=InlineKeyboardMarkup(keyboard)
        )
        return


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
