"""Bridge localhost HTTP proxy connections to Crow's restricted download socket."""
import asyncio

async def bridge(reader, writer):
    upstream = None
    async def copy(source, target):
        while data := await source.read(65536):
            target.write(data)
            await target.drain()
    try:
        remote, upstream = await asyncio.open_unix_connection('/run/crow-downloads/socket')
        tasks = [asyncio.create_task(copy(reader, upstream)), asyncio.create_task(copy(remote, writer))]
        try:
            await asyncio.wait(tasks, return_when=asyncio.FIRST_COMPLETED)
        finally:
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
    finally:
        writer.close()
        if upstream:
            upstream.close()

async def main():
    server = await asyncio.start_server(bridge, '127.0.0.1', 3128)
    async with server:
        await server.serve_forever()

asyncio.run(main())
