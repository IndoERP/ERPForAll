# /// script
# requires-python = ">=3.11"
# dependencies = ["mcp==1.30.0"]
# ///
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Exercise the official MCP SDK against the disposable native HTTP fixture."""
import asyncio
import json
import sys
import uuid

import httpx
from mcp import ClientSession
from mcp.client.streamable_http import streamable_http_client


async def verify(fixture):
    # Only the integration harness supplies this loopback port and its fixture credentials.
    origin = f"http://127.0.0.1:{int(fixture['port'])}"
    async with httpx.AsyncClient(base_url=origin, trust_env=False, timeout=15) as web:
        async def rpc(actor, model, method, **params):
            response = await web.post(
                f"/erp/json/2/{model}/{method}", json=params,
                headers={"Cookie": fixture[actor], "Sec-Fetch-Site": "same-origin", "Origin": origin},
            )
            response.raise_for_status()
            return response.json()

        response = await web.post("/erp/session/api-key", json={
            "audience": "mcp", "db": fixture["database"], "login": fixture["login"],
            "password": fixture["password"], "device_name": "SDK acceptance",
        })
        response.raise_for_status()
        key = response.json()["api_key"]
        activity, = await rpc("owner", "mail.activity", "create", vals_list=[{
            "user_id": fixture["user"], "summary": "SDK review", "date_deadline": "2030-01-01",
        }])
        tool = "mail.activity.action_feedback"
        args = {"ids": [activity], "feedback": "Reviewed using the SDK", "attachment_ids": []}
        headers = {"Authorization": "Bearer " + key}
        async with httpx.AsyncClient(headers=headers, trust_env=False, timeout=15) as client:
            async with streamable_http_client(origin + "/erp/mcp", http_client=client) as streams:
                async with ClientSession(*streams[:2]) as session:
                    await session.initialize()
                    names = {entry.name for entry in (await session.list_tools()).tools}
                    read = "res.company.action_all_company_branches"
                    assert {read, tool} <= names
                    assert not (await session.call_tool(read, {"ids": [fixture["company"]]})).isError
                    client.headers["Idempotency-Key"] = str(uuid.uuid4())
                    pending = await session.call_tool(tool, args)
                    assert pending.isError
                    request_id = pending.structuredContent["approval_request_id"]
                    before, = await rpc("owner", "mail.activity", "read", ids=[activity], fields=["active"])
                    assert before["active"]
                    await rpc("reviewer", "ir.approval.request", "erp_decide", request_id=request_id, approve=True)
                    for _ in range(2):
                        assert not (await session.call_tool(tool, args)).isError
                    after, = await rpc("reviewer", "mail.activity", "read", ids=[activity], fields=["active", "feedback", "write_uid"])
                    actor = after["write_uid"]
                    assert not after["active"] and after["feedback"] == args["feedback"]
                    assert (actor[0] if isinstance(actor, list) else actor) == fixture["user"]
                    assert await rpc("owner", "res.users.apikeys", "revoke", key=key)
                    revoked = await client.post(origin + "/erp/mcp", json={"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
                    assert revoked.status_code == 401


if __name__ == "__main__":
    asyncio.run(verify(json.load(sys.stdin)))
