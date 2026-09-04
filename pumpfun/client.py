"""
Python pump.fun trading client — buy/sell on bonding curve via buy_v2/sell_v2 instructions.
Ported from the Rust implementation in pumpfun-sniper-bot/src/executor/pumpfun.rs
"""

import os
import base64
import struct
import time
import asyncio
import logging
from typing import Optional, Dict, Tuple
from dataclasses import dataclass

import aiohttp
from solders.pubkey import Pubkey
from solders.keypair import Keypair
from solders.instruction import Instruction, AccountMeta
from solders.transaction import Transaction
from solders.hash import Hash
from solders.compute_budget import set_compute_unit_price, set_compute_unit_limit

logger = logging.getLogger(__name__)

# ═══════════════════════════════════════════════════════════════
# Constants
# ═══════════════════════════════════════════════════════════════

PUMP_FUN_PROGRAM = Pubkey.from_string("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P")
PUMP_GLOBAL = Pubkey.from_string("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf")
PUMP_EVENT_AUTHORITY = Pubkey.from_string("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1")
PUMP_FEE_PROGRAM = Pubkey.from_string("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ")
PUMP_GLOBAL_VOL_ACCUMULATOR = Pubkey.from_string("Hq2wp8uJ9jCPsYgNHex8RtqdvMPfVGoYwjvF1ATiwn2Y")

WSOL_MINT = Pubkey.from_string("So11111111111111111111111111111111111111112")
LEGACY_TOKEN_PROGRAM = Pubkey.from_string("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")
ASSOC_TOKEN_PROGRAM = Pubkey.from_string("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL")
SYSTEM_PROGRAM_ID = Pubkey.from_string("11111111111111111111111111111111")

BUY_V2_DISCRIMINATOR = bytes([0xb8, 0x17, 0xee, 0x61, 0x67, 0xc5, 0xd3, 0x3d])
SELL_V2_DISCRIMINATOR = bytes([0x5d, 0xf6, 0x82, 0x3c, 0xe7, 0xe9, 0x40, 0xb2])
INIT_USER_VOLUME_ACCUMULATOR_DISCRIMINATOR = bytes([0x5e, 0x06, 0xca, 0x73, 0xff, 0x60, 0xe8, 0xb7])

SOL_DECIMALS = 1_000_000_000
TOKEN_DECIMALS = 1_000_000
SLIPPAGE_BPS = 5000  # 50%

# Fee recipients (from Global account data, read live Jun 18 2026)
NORMAL_FEE_RECIPIENTS = [
    Pubkey.from_string("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV"),
    Pubkey.from_string("7VtfL8fvgNfhz17qKRMjzQEXgbdpnHHHQRh54R9jP2RJ"),
    Pubkey.from_string("7hTckgnGnLQR6sdH7YkqFTAA7VwTfYFaZ6EhEsU3saCX"),
    Pubkey.from_string("9rPYyANsfQZw3DnDmKE3YCQF5E8oD89UXoHn9JFEhJUz"),
    Pubkey.from_string("AVmoTthdrX6tKt4nDjco2D775W2YK3sDhxPcMmzUAmTY"),
    Pubkey.from_string("CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM"),
    Pubkey.from_string("FWsW1xNtWscwNmKv6wVsU1iTzRN6wmmk3MjxRP5tT7hz"),
    Pubkey.from_string("G5UZAVbAf46s7cKWoyKu8kYTip9DGTpbLZ2qa9Aq69dP"),
]

BUYBACK_FEE_RECIPIENTS = [
    Pubkey.from_string("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD"),
    Pubkey.from_string("9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7"),
    Pubkey.from_string("GXPFM2caqTtQYC2cJ5yJRi9VDkpsYZXzYdwYpGnLmtDL"),
    Pubkey.from_string("3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR"),
    Pubkey.from_string("5cjcW9wExnJJiqgLjq7DEG75Pm6JBgE1hNv4B2vHXUW6"),
    Pubkey.from_string("EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL"),
    Pubkey.from_string("5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD"),
    Pubkey.from_string("A7hAgCzFw14fejgCp387JUJRMNyz4j89JKnhtKU8piqW"),
]


@dataclass
class BondingCurveState:
    bonding_curve: str
    associated_bonding_curve: str
    virtual_sol_reserves: int
    virtual_token_reserves: int
    complete: bool
    token_program: str
    creator: str


@dataclass
class TokenInfo:
    mint: str
    symbol: str
    name: str
    creator: str
    price_sol: float  # per token
    market_cap_usd: float
    complete: bool


class PumpFunClient:
    """High-level pump.fun trading client."""

    def __init__(self, rpc_url: str, keypair: Keypair):
        self.rpc_url = rpc_url
        self.keypair = keypair
        self._session: Optional[aiohttp.ClientSession] = None
        self._blockhash_cache: Optional[Tuple[str, float]] = None
        self._blockhash_ttl = 30  # seconds

    async def _get_session(self) -> aiohttp.ClientSession:
        if self._session is None or self._session.closed:
            self._session = aiohttp.ClientSession(
                timeout=aiohttp.ClientTimeout(total=30)
            )
        return self._session

    async def close(self):
        if self._session and not self._session.closed:
            await self._session.close()

    async def _rpc_call(self, method: str, params: list) -> dict:
        session = await self._get_session()
        payload = {
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        }
        async with session.post(self.rpc_url, json=payload) as resp:
            data = await resp.json()
            if "error" in data:
                raise Exception(f"RPC error: {data['error']}")
            return data

    async def get_balance(self) -> float:
        """Get wallet balance in SOL."""
        result = await self._rpc_call(
            "getBalance",
            [str(self.keypair.pubkey()), {"commitment": "confirmed"}]
        )
        lamports = result["result"]["value"]
        return lamports / SOL_DECIMALS

    async def get_token_balance(self, mint: str) -> float:
        """Get token balance for a specific mint in UI units."""
        # Get the ATA
        mint_pubkey = Pubkey.from_string(mint)
        ata = self._get_associated_token_address(self.keypair.pubkey(), mint_pubkey, LEGACY_TOKEN_PROGRAM)
        result = await self._rpc_call(
            "getTokenAccountBalance",
            [str(ata)]
        )
        if "result" in result and result["result"]:
            return result["result"]["value"]["uiAmount"]
        return 0.0

    async def get_recent_blockhash(self) -> str:
        """Get recent blockhash with caching."""
        if self._blockhash_cache:
            blockhash, cached_at = self._blockhash_cache
            if time.time() - cached_at < self._blockhash_ttl:
                return blockhash
        result = await self._rpc_call(
            "getLatestBlockhash",
            [{"commitment": "finalized"}]
        )
        blockhash = result["result"]["value"]["blockhash"]
        self._blockhash_cache = (blockhash, time.time())
        return blockhash

    async def fetch_bonding_curve(self, mint: str) -> BondingCurveState:
        """Fetch bonding curve state via RPC."""
        mint_pubkey = Pubkey.from_string(mint)

        # Derive bonding curve PDA
        bonding_curve_pubkey, _ = Pubkey.find_program_address(
            [b"bonding-curve", mint_pubkey.as_ref()],
            PUMP_FUN_PROGRAM,
        )

        # Fetch mint + bonding curve in parallel
        session = await self._get_session()
        payloads = [
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getAccountInfo",
                "params": [mint, {"encoding": "jsonParsed"}],
            },
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "getAccountInfo",
                "params": [str(bonding_curve_pubkey), {"encoding": "base64"}],
            },
        ]
        async with session.post(self.rpc_url, json=payloads[0]) as resp1, \
                   session.post(self.rpc_url, json=payloads[1]) as resp2:
            mint_json = await resp1.json()
            curve_json = await resp2.json()

        # Extract token program from mint account owner
        mint_value = mint_json.get("result", {}).get("value")
        if not mint_value:
            raise Exception(f"Mint {mint} does not exist on-chain")
        token_program = mint_value["owner"]

        # Parse bonding curve data
        curve_value = curve_json.get("result", {}).get("value")
        if not curve_value:
            raise Exception(f"Bonding curve does not exist for {mint}")

        curve_owner = curve_value.get("owner")
        if curve_owner != str(PUMP_FUN_PROGRAM):
            raise Exception(f"Bonding curve owned by {curve_owner}, not pump.fun")

        data_b64 = curve_value["data"][0]
        data = base64.b64decode(data_b64)

        if len(data) < 49:
            raise Exception(f"Bonding curve data too short: {len(data)} bytes")

        # Skip 8-byte Anchor discriminator
        virtual_token_reserves = struct.unpack_from("<Q", data, 8)[0]
        virtual_sol_reserves = struct.unpack_from("<Q", data, 16)[0]
        complete = data[48] != 0

        # Creator field (offset 49..81 for new layout)
        if len(data) >= 81:
            creator_bytes = data[49:81]
            if creator_bytes == b"\x00" * 32:
                creator = str(Pubkey.default())
            else:
                creator = str(Pubkey.from_bytes(creator_bytes))
        else:
            creator = str(Pubkey.default())

        # Associated bonding curve (ATA of bonding curve)
        assoc_bonding_curve = self._get_associated_token_address(
            bonding_curve_pubkey, mint_pubkey, Pubkey.from_string(token_program)
        )

        return BondingCurveState(
            bonding_curve=str(bonding_curve_pubkey),
            associated_bonding_curve=str(assoc_bonding_curve),
            virtual_sol_reserves=virtual_sol_reserves,
            virtual_token_reserves=virtual_token_reserves,
            complete=complete,
            token_program=token_program,
            creator=creator,
        )

    async def buy_token(self, mint: str, amount_sol: float, 
                        priority_fee_cu: int = 200_000,
                        priority_fee_lamports: int = 100_000) -> str:
        """Buy a token on pump.fun bonding curve. Returns tx signature."""
        mint_pubkey = Pubkey.from_string(mint)
        curve = await self.fetch_bonding_curve(mint)

        if curve.complete:
            raise Exception("Token has graduated to PumpSwap")

        sol_lamports = int(amount_sol * SOL_DECIMALS)

        # Calculate expected tokens with slippage
        expected_tokens = (
            sol_lamports * curve.virtual_token_reserves
        ) // max(curve.virtual_sol_reserves, 1)
        min_tokens = expected_tokens * (10000 - SLIPPAGE_BPS) // 10000
        max_sol_cost = sol_lamports + (sol_lamports * SLIPPAGE_BPS // 10000)

        # Build instructions
        instructions = []

        # Priority fee
        instructions.append(set_compute_unit_price(priority_fee_lamports))
        instructions.append(set_compute_unit_limit(priority_fee_cu))

        # Init user volume accumulator (required for new wallets)
        instructions.append(self._build_init_user_volume_accumulator())

        # Create ATA
        token_prog_pubkey = Pubkey.from_string(curve.token_program)
        instructions.append(self._build_create_idempotent_ata(mint_pubkey, token_prog_pubkey))

        # Buy v2
        creator_pubkey = Pubkey.from_string(curve.creator) if curve.creator != str(Pubkey.default()) else Pubkey.default()
        bonding_curve_pubkey = Pubkey.from_string(curve.bonding_curve)
        assoc_bonding_curve_pubkey = Pubkey.from_string(curve.associated_bonding_curve)

        user_ata = self._get_associated_token_address(
            self.keypair.pubkey(), mint_pubkey, token_prog_pubkey
        )
        instructions.append(self._build_buy_v2(
            mint_pubkey, bonding_curve_pubkey, assoc_bonding_curve_pubkey,
            user_ata, min_tokens, max_sol_cost, token_prog_pubkey, creator_pubkey
        ))

        # Build and send transaction
        blockhash_str = await self.get_recent_blockhash()
        blockhash = Hash.from_string(blockhash_str)

        tx = Transaction.new_signed_with_payer(
            instructions,
            self.keypair.pubkey(),
            [self.keypair],
            blockhash,
        )

        return await self._send_transaction(tx)

    async def sell_token(self, mint: str, token_amount: float,
                         priority_fee_cu: int = 200_000,
                         priority_fee_lamports: int = 100_000) -> str:
        """Sell a token on pump.fun bonding curve. Returns tx signature."""
        mint_pubkey = Pubkey.from_string(mint)
        raw_amount = int(token_amount * TOKEN_DECIMALS)

        if raw_amount == 0:
            raise Exception("Sell amount too small")

        curve = await self.fetch_bonding_curve(mint)

        if curve.complete:
            raise Exception("Token has graduated to PumpSwap")

        # Calculate expected SOL output
        expected_sol = (
            raw_amount * curve.virtual_sol_reserves
        ) // (curve.virtual_token_reserves + raw_amount)
        min_sol = expected_sol * (10000 - SLIPPAGE_BPS) // 10000

        # Build instructions
        instructions = []
        instructions.append(set_compute_unit_price(priority_fee_lamports))
        instructions.append(set_compute_unit_limit(priority_fee_cu))

        creator_pubkey = Pubkey.from_string(curve.creator) if curve.creator != str(Pubkey.default()) else Pubkey.default()
        bonding_curve_pubkey = Pubkey.from_string(curve.bonding_curve)
        assoc_bonding_curve_pubkey = Pubkey.from_string(curve.associated_bonding_curve)
        token_prog_pubkey = Pubkey.from_string(curve.token_program)

        user_ata = self._get_associated_token_address(
            self.keypair.pubkey(), mint_pubkey, token_prog_pubkey
        )
        instructions.append(self._build_sell_v2(
            mint_pubkey, bonding_curve_pubkey, assoc_bonding_curve_pubkey,
            user_ata, raw_amount, min_sol, token_prog_pubkey, creator_pubkey
        ))

        # Build and send transaction
        blockhash_str = await self.get_recent_blockhash()
        blockhash = Hash.from_string(blockhash_str)

        tx = Transaction.new_signed_with_payer(
            instructions,
            self.keypair.pubkey(),
            [self.keypair],
            blockhash,
        )

        return await self._send_transaction(tx)

    async def get_token_info(self, mint: str) -> Optional[TokenInfo]:
        """Fetch token info from pump.fun API."""
        session = await self._get_session()
        url = f"https://frontend-api-v3.pump.fun/coins/{mint}"
        try:
            async with session.get(url, headers={
                "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64)"
            }) as resp:
                if resp.status != 200:
                    return None
                data = await resp.json()

            virtual_sol = float(data.get("virtual_sol_reserves", 0))
            virtual_token = float(data.get("virtual_token_reserves", 1))
            price_sol = virtual_sol / virtual_token if virtual_token > 0 else 0

            # Rough market cap estimate
            price_usd = price_sol * 150  # approximate SOL price
            total_supply = float(data.get("total_supply", 1_000_000_000))
            market_cap_usd = total_supply * price_usd

            return TokenInfo(
                mint=mint,
                symbol=data.get("symbol", "???"),
                name=data.get("name", "Unknown"),
                creator=data.get("creator", "unknown"),
                price_sol=price_sol,
                market_cap_usd=market_cap_usd,
                complete=data.get("complete", False),
            )
        except Exception as e:
            logger.warning(f"Failed to fetch token info for {mint}: {e}")
            return None

    async def get_new_tokens(self, limit: int = 20) -> list:
        """Fetch newest tokens from pump.fun API."""
        session = await self._get_session()
        url = "https://frontend-api-v3.pump.fun/coins"
        try:
            async with session.get(url, params={
                "limit": limit,
                "offset": 0,
                "sort": "created_timestamp",
                "order": "DESC",
            }, headers={
                "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64)"
            }) as resp:
                if resp.status != 200:
                    return []
                data = await resp.json()
            return data if isinstance(data, list) else []
        except Exception as e:
            logger.warning(f"Failed to fetch new tokens: {e}")
            return []

    # ═══════════════════════════════════════════════════════════════
    # Instruction Builders
    # ═══════════════════════════════════════════════════════════════

    def _build_buy_v2(self, mint: Pubkey, bonding_curve: Pubkey,
                      assoc_bonding_curve: Pubkey, user_ata: Pubkey,
                      amount: int, max_sol_cost: int,
                      token_prog: Pubkey, creator: Pubkey) -> Instruction:
        """Build buy_v2 instruction (27 accounts)."""
        creator_vault, _ = Pubkey.find_program_address(
            [b"creator-vault", creator.as_ref()], PUMP_FUN_PROGRAM
        )
        user_vol_accumulator, _ = Pubkey.find_program_address(
            [b"user_volume_accumulator", self.keypair.pubkey().as_ref()], PUMP_FUN_PROGRAM
        )
        fee_config, _ = Pubkey.find_program_address(
            [b"fee_config", PUMP_FUN_PROGRAM.as_ref()], PUMP_FEE_PROGRAM
        )
        sharing_config, _ = Pubkey.find_program_address(
            [b"sharing-config", mint.as_ref()], PUMP_FEE_PROGRAM
        )

        fee_recipient = NORMAL_FEE_RECIPIENTS[0]
        buyback_fee_recipient = BUYBACK_FEE_RECIPIENTS[5]

        # Associated token accounts
        assoc_quote_fee_recipient, _ = Pubkey.find_program_address(
            [fee_recipient.as_ref(), LEGACY_TOKEN_PROGRAM.as_ref(), WSOL_MINT.as_ref()],
            ASSOC_TOKEN_PROGRAM
        )
        assoc_quote_buyback_fee_recipient, _ = Pubkey.find_program_address(
            [buyback_fee_recipient.as_ref(), LEGACY_TOKEN_PROGRAM.as_ref(), WSOL_MINT.as_ref()],
            ASSOC_TOKEN_PROGRAM
        )
        assoc_quote_bonding_curve, _ = Pubkey.find_program_address(
            [bonding_curve.as_ref(), LEGACY_TOKEN_PROGRAM.as_ref(), WSOL_MINT.as_ref()],
            ASSOC_TOKEN_PROGRAM
        )
        assoc_quote_user, _ = Pubkey.find_program_address(
            [self.keypair.pubkey().as_ref(), LEGACY_TOKEN_PROGRAM.as_ref(), WSOL_MINT.as_ref()],
            ASSOC_TOKEN_PROGRAM
        )
        assoc_creator_vault, _ = Pubkey.find_program_address(
            [creator_vault.as_ref(), LEGACY_TOKEN_PROGRAM.as_ref(), WSOL_MINT.as_ref()],
            ASSOC_TOKEN_PROGRAM
        )
        assoc_user_volume_accumulator, _ = Pubkey.find_program_address(
            [user_vol_accumulator.as_ref(), LEGACY_TOKEN_PROGRAM.as_ref(), WSOL_MINT.as_ref()],
            ASSOC_TOKEN_PROGRAM
        )

        # Data: discriminator + amount (u64) + max_sol_cost (u64)
        data = BUY_V2_DISCRIMINATOR + struct.pack("<Q", amount) + struct.pack("<Q", max_sol_cost)

        accounts = [
            AccountMeta(PUMP_GLOBAL, False, False),
            AccountMeta(mint, False, False),
            AccountMeta(WSOL_MINT, False, False),
            AccountMeta(token_prog, False, False),
            AccountMeta(LEGACY_TOKEN_PROGRAM, False, False),
            AccountMeta(ASSOC_TOKEN_PROGRAM, False, False),
            AccountMeta(fee_recipient, True, False),
            AccountMeta(assoc_quote_fee_recipient, True, False),
            AccountMeta(buyback_fee_recipient, True, False),
            AccountMeta(assoc_quote_buyback_fee_recipient, True, False),
            AccountMeta(bonding_curve, True, False),
            AccountMeta(assoc_bonding_curve, True, False),
            AccountMeta(assoc_quote_bonding_curve, True, False),
            AccountMeta(self.keypair.pubkey(), True, True),
            AccountMeta(user_ata, True, False),
            AccountMeta(assoc_quote_user, True, False),
            AccountMeta(creator_vault, True, False),
            AccountMeta(assoc_creator_vault, True, False),
            AccountMeta(sharing_config, False, False),
            AccountMeta(PUMP_GLOBAL_VOL_ACCUMULATOR, False, False),
            AccountMeta(user_vol_accumulator, True, False),
            AccountMeta(assoc_user_volume_accumulator, True, False),
            AccountMeta(fee_config, False, False),
            AccountMeta(PUMP_FEE_PROGRAM, False, False),
            AccountMeta(SYSTEM_PROGRAM_ID, False, False),
            AccountMeta(PUMP_EVENT_AUTHORITY, False, False),
            AccountMeta(PUMP_FUN_PROGRAM, False, False),
        ]

        return Instruction(PUMP_FUN_PROGRAM, accounts, data)

    def _build_sell_v2(self, mint: Pubkey, bonding_curve: Pubkey,
                       assoc_bonding_curve: Pubkey, user_ata: Pubkey,
                       amount: int, min_sol_output: int,
                       token_prog: Pubkey, creator: Pubkey) -> Instruction:
        """Build sell_v2 instruction (26 accounts)."""
        creator_vault, _ = Pubkey.find_program_address(
            [b"creator-vault", creator.as_ref()], PUMP_FUN_PROGRAM
        )
        user_vol_accumulator, _ = Pubkey.find_program_address(
            [b"user_volume_accumulator", self.keypair.pubkey().as_ref()], PUMP_FUN_PROGRAM
        )
        fee_config, _ = Pubkey.find_program_address(
            [b"fee_config", PUMP_FUN_PROGRAM.as_ref()], PUMP_FEE_PROGRAM
        )
        sharing_config, _ = Pubkey.find_program_address(
            [b"sharing-config", mint.as_ref()], PUMP_FEE_PROGRAM
        )

        fee_recipient = NORMAL_FEE_RECIPIENTS[0]
        buyback_fee_recipient = BUYBACK_FEE_RECIPIENTS[5]

        assoc_quote_fee_recipient, _ = Pubkey.find_program_address(
            [fee_recipient.as_ref(), LEGACY_TOKEN_PROGRAM.as_ref(), WSOL_MINT.as_ref()],
            ASSOC_TOKEN_PROGRAM
        )
        assoc_quote_buyback_fee_recipient, _ = Pubkey.find_program_address(
            [buyback_fee_recipient.as_ref(), LEGACY_TOKEN_PROGRAM.as_ref(), WSOL_MINT.as_ref()],
            ASSOC_TOKEN_PROGRAM
        )
        assoc_quote_bonding_curve, _ = Pubkey.find_program_address(
            [bonding_curve.as_ref(), LEGACY_TOKEN_PROGRAM.as_ref(), WSOL_MINT.as_ref()],
            ASSOC_TOKEN_PROGRAM
        )
        assoc_creator_vault, _ = Pubkey.find_program_address(
            [creator_vault.as_ref(), LEGACY_TOKEN_PROGRAM.as_ref(), WSOL_MINT.as_ref()],
            ASSOC_TOKEN_PROGRAM
        )
        assoc_user_volume_accumulator, _ = Pubkey.find_program_address(
            [user_vol_accumulator.as_ref(), LEGACY_TOKEN_PROGRAM.as_ref(), WSOL_MINT.as_ref()],
            ASSOC_TOKEN_PROGRAM
        )
        assoc_quote_user, _ = Pubkey.find_program_address(
            [self.keypair.pubkey().as_ref(), LEGACY_TOKEN_PROGRAM.as_ref(), WSOL_MINT.as_ref()],
            ASSOC_TOKEN_PROGRAM
        )

        data = SELL_V2_DISCRIMINATOR + struct.pack("<Q", amount) + struct.pack("<Q", min_sol_output)

        accounts = [
            AccountMeta(PUMP_GLOBAL, False, False),
            AccountMeta(mint, False, False),
            AccountMeta(WSOL_MINT, False, False),
            AccountMeta(token_prog, False, False),
            AccountMeta(LEGACY_TOKEN_PROGRAM, False, False),
            AccountMeta(ASSOC_TOKEN_PROGRAM, False, False),
            AccountMeta(fee_recipient, True, False),
            AccountMeta(assoc_quote_fee_recipient, True, False),
            AccountMeta(buyback_fee_recipient, True, False),
            AccountMeta(assoc_quote_buyback_fee_recipient, True, False),
            AccountMeta(bonding_curve, True, False),
            AccountMeta(assoc_bonding_curve, True, False),
            AccountMeta(assoc_quote_bonding_curve, True, False),
            AccountMeta(self.keypair.pubkey(), True, True),
            AccountMeta(user_ata, True, False),
            AccountMeta(assoc_quote_user, True, False),
            AccountMeta(creator_vault, True, False),
            AccountMeta(assoc_creator_vault, True, False),
            AccountMeta(sharing_config, False, False),
            AccountMeta(user_vol_accumulator, True, False),
            AccountMeta(assoc_user_volume_accumulator, True, False),
            AccountMeta(fee_config, False, False),
            AccountMeta(PUMP_FEE_PROGRAM, False, False),
            AccountMeta(SYSTEM_PROGRAM_ID, False, False),
            AccountMeta(PUMP_EVENT_AUTHORITY, False, False),
            AccountMeta(PUMP_FUN_PROGRAM, False, False),
        ]

        return Instruction(PUMP_FUN_PROGRAM, accounts, data)

    def _build_init_user_volume_accumulator(self) -> Instruction:
        """Build initUserVolumeAccumulator instruction."""
        user_vol_accumulator, _ = Pubkey.find_program_address(
            [b"user_volume_accumulator", self.keypair.pubkey().as_ref()],
            PUMP_FUN_PROGRAM,
        )
        accounts = [
            AccountMeta(self.keypair.pubkey(), True, True),
            AccountMeta(self.keypair.pubkey(), False, False),
            AccountMeta(user_vol_accumulator, True, False),
            AccountMeta(SYSTEM_PROGRAM_ID, False, False),
            AccountMeta(PUMP_EVENT_AUTHORITY, False, False),
            AccountMeta(PUMP_FUN_PROGRAM, False, False),
        ]
        return Instruction(PUMP_FUN_PROGRAM, accounts, INIT_USER_VOLUME_ACCUMULATOR_DISCRIMINATOR)

    def _build_create_idempotent_ata(self, mint: Pubkey, token_prog: Pubkey) -> Instruction:
        """Build create_idempotent ATA instruction."""
        ata = self._get_associated_token_address(self.keypair.pubkey(), mint, token_prog)
        accounts = [
            AccountMeta(self.keypair.pubkey(), True, True),
            AccountMeta(ata, True, False),
            AccountMeta(self.keypair.pubkey(), False, False),
            AccountMeta(mint, False, False),
            AccountMeta(SYSTEM_PROGRAM_ID, False, False),
            AccountMeta(token_prog, False, False),
        ]
        return Instruction(ASSOC_TOKEN_PROGRAM, accounts, bytes([1]))

    # ═══════════════════════════════════════════════════════════════
    # Transaction Sending
    # ═══════════════════════════════════════════════════════════════

    async def _send_transaction(self, tx: Transaction) -> str:
        """Send a transaction and wait for confirmation."""
        session = await self._get_session()
        encoded = base64.b64encode(bytes(tx)).decode()

        # Send
        send_payload = {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [
                encoded,
                {
                    "encoding": "base64",
                    "skipPreflight": True,
                    "maxRetries": 3,
                    "preflightCommitment": "processed",
                },
            ],
        }
        start = time.time()
        async with session.post(self.rpc_url, json=send_payload) as resp:
            result = await resp.json()

        if "error" in result:
            raise Exception(f"Send error: {result['error']}")

        sig = result["result"]
        elapsed = (time.time() - start) * 1000
        logger.info(f"TX submitted in {elapsed:.0f}ms: {sig}")

        # Confirm
        await self._confirm_transaction(sig)
        return sig

    async def _confirm_transaction(self, sig: str, timeout: int = 30):
        """Poll for transaction confirmation."""
        session = await self._get_session()
        for _ in range(timeout):
            await asyncio.sleep(1)
            payload = {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "getSignatureStatuses",
                "params": [[sig], {"searchTransactionHistory": True}],
            }
            async with session.post(self.rpc_url, json=payload) as resp:
                result = await resp.json()
            status = result.get("result", {}).get("value", [None])[0]
            if status and status.get("confirmationStatus") in ("confirmed", "finalized"):
                if status.get("err"):
                    raise Exception(f"Transaction failed: {status['err']}")
                return
        raise Exception(f"Transaction {sig} not confirmed after {timeout}s")

    # ═══════════════════════════════════════════════════════════════
    # Helpers
    # ═══════════════════════════════════════════════════════════════

    @staticmethod
    def _get_associated_token_address(wallet: Pubkey, mint: Pubkey, token_prog: Pubkey) -> Pubkey:
        """Derive associated token address."""
        ata, _ = Pubkey.find_program_address(
            [wallet.as_ref(), token_prog.as_ref(), mint.as_ref()],
            ASSOC_TOKEN_PROGRAM,
        )
        return ata
