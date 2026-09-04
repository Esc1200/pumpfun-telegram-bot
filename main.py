"""
Entry point for the Telegram pump.fun trading bot.
"""

import asyncio
import logging
from bot import PumpFunBot

logging.basicConfig(
    format="%(asctime)s - %(name)s - %(levelname)s - %(message)s",
    level=logging.INFO
)

async def main():
    bot = PumpFunBot()
    await bot.run()

if __name__ == "__main__":
    asyncio.run(main())
