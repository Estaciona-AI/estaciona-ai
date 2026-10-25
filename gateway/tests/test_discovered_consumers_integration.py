# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Synthetic consumers discover a real core; no customer panel or ML is mocked in."""

import asyncio
import datetime
import json
import os
import time
from urllib.parse import urlencode
from uuid import UUID, uuid4

import pytest
import websockets
from jsonschema import ValidationError
from websockets.exceptions import InvalidStatus

from contract_validation import ContractValidator

from test_delivery_integration import KEY, server as delivery_server
from test_rest_contract_integration import call
from test_session_integration import insert_user, token

server = delivery_server
pytestmark = pytest.mark.skipif(
    not os.environ.get("ESTACIONA_TEST_SERVER"), reason="integration server required"
)


class Consumer:
    """Test-only client bootstrapped from the existing public discovery endpoint."""

    def __init__(self, server):
        self.server = server
        status, _, raw = call(server, "/api/v1/capabilities")
        assert status == 200
        self.capabilities = json.loads(raw)
        self.manifest = self.get_json(self.capabilities["paths"]["contract_manifest"])
        self.catalog = self.artifact(self.manifest["catalog"])
        assert self.manifest["version"] == self.catalog["version"]
        assert self.catalog["version"] == self.capabilities["contract_version"]
        self.operations = {item["id"]: item for item in self.catalog["http"]}
        self.sockets = {item["id"]: item for item in self.catalog["websockets"]}
        documents = {"messages.json": self.artifact("messages.json")}
        for name, uri in self.manifest["channels"].items():
            path = f"v1/{name}.schema.json"
            documents[path] = self.artifact(path)
            assert documents[path]["$id"] == uri
        for item in self.manifest["rest"]["schemas"].values():
            documents[item["path"]] = self.artifact(item["path"])
            assert documents[item["path"]]["$id"] == item["id"]
        self.validator = ContractValidator(documents)
        self.validator.validate(
            self.operations["capabilities"]["responses"]["200"]["schema"],
            self.capabilities,
        )

    def get_json(self, path):
        status, _, body = call(self.server, path)
        assert status == 200, body
        return json.loads(body)

    def artifact(self, path):
        return self.get_json(f"/api/v1/contracts/{path}")

    def request(
        self, operation, *, payload=None, bearer=None, parameters=None, query=None
    ):
        operation = self.operations[operation]
        path = operation["path"].format(**(parameters or {}))
        if query:
            path += "?" + urlencode(query)
        result = call(
            self.server,
            path,
            operation["method"],
            payload,
            bearer,
            {"Origin": "http://localhost"},
        )
        status, headers, raw = result
        if 200 <= status < 300:
            response = operation["responses"][str(status)]
            if response["encoding"] == "empty":
                assert raw == b""
            elif response["encoding"] == "json":
                assert headers["Content-Type"].startswith("application/json")
                if response["schema"] is not None:
                    self.validator.validate(
                        response["schema"],
                        json.loads(raw),
                        collection=response.get("collection", False),
                    )
                else:
                    json.loads(raw)
            else:
                assert headers["Content-Type"].startswith("text/plain")
        elif status >= 400 and operation["error_schema"] is not None:
            assert headers["Content-Type"].startswith("application/json")
            self.validator.validate(operation["error_schema"], json.loads(raw))
        return result

    def ticket_url(self, destination, bearer):
        socket = self.sockets[destination]
        status, headers, raw = self.request(
            "websocket_ticket",
            payload={"destination": socket["ticket_destination"]},
            bearer=bearer,
        )
        assert status == 201, raw
        assert headers["Cache-Control"] == "no-store"
        ticket = json.loads(raw)["ticket"]
        return f"ws://{self.server.host}:8000{socket['path']}?{urlencode({'ticket': ticket})}"

    def signed_in(self):
        """An account provisioned at the identity provider, after its sign-in."""
        user_id = insert_user(self.server)
        bearer = token(self.server, user_id, "user", int(time.time()) + 3600)
        status, _, raw = self.request("current_user", bearer=bearer)
        assert status == 200, raw
        assert json.loads(raw)["id"] == str(user_id)
        return str(user_id), bearer


@pytest.fixture
def consumer(server):
    return Consumer(server)


async def receive(socket, kind, consumer, channel):
    async with asyncio.timeout(5):
        while True:
            message = json.loads(await socket.recv())
            consumer.validator.validate(
                consumer.sockets[channel]["send"]["schema"], message
            )
            if message["type"] == kind:
                return message


def assert_error(status, headers, raw, expected_status, expected_code):
    assert status == expected_status, raw
    error = json.loads(raw)
    assert error["code"] == expected_code
    assert error["request_id"] == headers["X-Request-ID"]
    UUID(error["request_id"])
    assert headers["X-Estaciona-Api-Version"] == "1"


def test_consumer_resolves_all_advertised_schemas_from_live_server(consumer):
    paths = set()

    def collect(value):
        if isinstance(value, dict):
            for key, child in value.items():
                if key in {"schema", "request_schema", "error_schema"} and child:
                    paths.add(child)
                else:
                    collect(child)
        elif isinstance(value, list):
            for child in value:
                collect(child)

    collect(consumer.catalog)
    for path in paths:
        schema = consumer.artifact(path)
        assert schema["$schema"] == "https://json-schema.org/draft/2020-12/schema"
        assert schema["x-contract-version"] == consumer.manifest["version"]
    assert len(paths) >= 15
    assert consumer.capabilities["authentication"]["sso"] is True


def test_catalog_protected_operations_reject_anonymous_consumers(consumer):
    for name, operation in consumer.operations.items():
        if (
            operation["availability"] != "always"
            or operation["authentication"] == "public"
        ):
            continue
        payload = {
            "websocket_ticket": {"destination": "app"},
            "create_reservation": {"spot_id": "R02-A"},
            "update_spot_status": {"status": "free"},
            "update_eligibility": {"date_of_birth": None, "pcd_status": None},
            "save_config": [],
        }.get(name)
        result = consumer.request(
            name,
            payload=payload,
            parameters={"id": str(uuid4())},
            query={"user_id": str(uuid4())} if name == "recommend_spot" else None,
        )
        assert_error(*result, 401, "UNAUTHORIZED")
    for name, operation in consumer.operations.items():
        if operation["availability"] == "lpr_enabled":
            assert (
                consumer.request(
                    name,
                    parameters={"id": str(uuid4())},
                    payload={} if operation["method"] == "POST" else None,
                )[0]
                == 404
            )


def test_browser_consumer_login_reservation_ownership_and_logout(consumer):
    user_id, bearer = consumer.signed_in()
    _, other_bearer = consumer.signed_in()
    status, _, raw = consumer.request(
        "create_reservation", payload={"spot_id": "R02-A"}, bearer=bearer
    )
    assert status == 200, raw
    reservation = json.loads(raw)
    assert reservation["user_id"] == user_id
    assert reservation["status"] == "active"
    # A valid session belonging to another user must not manage the reservation.
    assert_error(
        *consumer.request(
            "cancel_reservation",
            bearer=other_bearer,
            parameters={"id": reservation["id"]},
        ),
        404,
        "NOT_FOUND",
    )
    assert_error(
        *consumer.request("list_reservations", bearer=bearer), 403, "FORBIDDEN"
    )
    status, headers, raw = consumer.request(
        "cancel_reservation", bearer=bearer, parameters={"id": reservation["id"]}
    )
    assert status == 200, raw
    assert headers["Content-Type"].startswith("text/plain")
    assert raw.decode() == "Reservation cancelled."
    assert consumer.server.query(
        "SELECT status FROM reservations WHERE id=%s", (reservation["id"],)
    ) == [("cancelled",)]
    status, _, raw = consumer.request("logout", bearer=bearer)
    assert status == 200 and json.loads(raw) == {"end_session_url": None}
    assert_error(
        *consumer.request("get_user", bearer=bearer, parameters={"id": user_id}),
        401,
        "UNAUTHORIZED",
    )


def test_browser_consumer_ticket_is_single_use_and_commands_use_session_identity(
    consumer,
):
    user_id, bearer = consumer.signed_in()
    assert_error(
        *consumer.request(
            "websocket_ticket", payload={"destination": "dashboard"}, bearer=bearer
        ),
        403,
        "FORBIDDEN",
    )
    url = consumer.ticket_url("app", bearer)

    async def run():
        async with websockets.connect(url, origin="http://localhost") as socket:
            snapshot = await receive(socket, "SPOT_UPDATE", consumer, "app")
            assert snapshot == {
                "type": "SPOT_UPDATE",
                "spot_id": "R02-A",
                "status": "free",
            }
            await socket.send(
                json.dumps(
                    {
                        "type": "RESERVE_SPOT",
                        "spot_id": "R02-A",
                        "user_id": str(uuid4()),
                    }
                )
            )
            confirmed = await receive(socket, "RESERVATION_CONFIRMED", consumer, "app")
            assert confirmed["spot_id"] == "R02-A"
            assert consumer.server.query(
                "SELECT user_id::text FROM reservations WHERE id=%s",
                (confirmed["reservation_id"],),
            ) == [(user_id,)]
            await socket.send(json.dumps({"type": "RESERVE_SPOT", "spot_id": "R02-A"}))
            rejected = await receive(socket, "RESERVATION_REJECTED", consumer, "app")
            assert rejected["reason"] in {"active_reservation_limit", "already_taken"}
        with pytest.raises(InvalidStatus) as reused:
            async with websockets.connect(url, origin="http://localhost"):
                pytest.fail("A used ticket opened another connection")
        assert reused.value.response.status_code == 401

    asyncio.run(run())


def test_edge_and_dashboard_consumers_observe_committed_occupancy_and_forecast(
    consumer,
):
    admin_id = insert_user(consumer.server, role="admin")
    bearer = token(consumer.server, admin_id, "admin", int(time.time()) + 120)
    dashboard_url = consumer.ticket_url("dashboard", bearer)
    edge_url = f"ws://{consumer.server.host}:8000{consumer.sockets['edge']['path']}"
    now = datetime.datetime.now(datetime.UTC)
    spot = {
        "type": "SPOT_UPDATE",
        "spot_id": "R02-A",
        "status": "occupied",
        "camera_id": "test-camera",
        "confidence": 0.9,
        "timestamp": now.isoformat().replace("+00:00", "Z"),
    }
    # Missing evaluation metrics stay explicitly unavailable; no fabricated accuracy.
    forecast = {
        "type": "TREND_PREDICTION_V2",
        "timestamp": now.isoformat().replace("+00:00", "Z"),
        "max_capacity": 1,
        "evaluation": {
            "status": "unavailable",
            "reason": "evaluation_failed",
            "inference_time_ms": 1.0,
        },
        "stay_duration": {"status": "unavailable", "reason": "not_measured"},
        "next_24h_occupancy": [
            {
                "timestamp": (now + datetime.timedelta(hours=1))
                .isoformat()
                .replace("+00:00", "Z"),
                "occupancy": 1,
            }
        ],
    }

    async def run():
        async with websockets.connect(
            dashboard_url, origin="http://localhost"
        ) as dashboard:
            assert (await receive(dashboard, "SPOT_UPDATE", consumer, "dashboard"))[
                "status"
            ] == "free"
            with pytest.raises(InvalidStatus) as denied:
                async with websockets.connect(edge_url):
                    pytest.fail("Anonymous edge connected")
            assert denied.value.response.status_code == 401
            async with websockets.connect(
                edge_url, additional_headers={"Authorization": f"Bearer {KEY}"}
            ) as edge:
                ready = await receive(edge, "EDGE_READY", consumer, "edge")
                assert "SPOT_UPDATE" in ready["durable_events"]
                assert "TREND_PREDICTION_V2" in ready["durable_events"]
                frame = {
                    "type": "EDGE_EVENT",
                    "version": 1,
                    "event_id": str(uuid4()),
                    "payload": spot,
                }
                await edge.send(json.dumps(frame))
                ack = await receive(edge, "EDGE_ACK", consumer, "edge")
                assert (
                    ack["event_id"] == frame["event_id"] and ack["outcome"] == "applied"
                )
                assert consumer.server.query(
                    "SELECT status FROM spots WHERE id='R02-A'"
                ) == [("occupied",)]
                assert (await receive(dashboard, "SPOT_UPDATE", consumer, "dashboard"))[
                    "status"
                ] == "occupied"
                await edge.send(json.dumps(frame))
                assert (await receive(edge, "EDGE_ACK", consumer, "edge"))[
                    "duplicate"
                ] is True
                forbidden = {
                    **frame,
                    "event_id": str(uuid4()),
                    "payload": {**spot, "camera_id": "ungranted-camera"},
                }
                await edge.send(json.dumps(forbidden))
                rejection = await receive(edge, "EDGE_ACK", consumer, "edge")
                assert (
                    rejection["outcome"] == "rejected"
                    and rejection["reason"] == "scope_forbidden"
                )
                assert consumer.server.query(
                    "SELECT status FROM spots WHERE id='R02-A'"
                ) == [("occupied",)]
                denied_forecast = {
                    **frame,
                    "event_id": str(uuid4()),
                    "payload": forecast,
                }
                await edge.send(json.dumps(denied_forecast))
                assert (await receive(edge, "EDGE_ACK", consumer, "edge"))[
                    "outcome"
                ] == "rejected"
                consumer.server.query(
                    "UPDATE edge_gateways SET trend_prediction=TRUE WHERE id=%s",
                    (consumer.server.gateway_id,),
                )
                allowed_forecast = {**denied_forecast, "event_id": str(uuid4())}
                await edge.send(json.dumps(allowed_forecast))
                ack = await receive(edge, "EDGE_ACK", consumer, "edge")
                assert (
                    ack["outcome"] == "applied"
                    and ack["event_id"] == allowed_forecast["event_id"]
                )
                assert (
                    await receive(
                        dashboard, "TREND_PREDICTION_V2", consumer, "dashboard"
                    )
                    == forecast
                )
                receipt = consumer.server.query(
                    "SELECT outcome FROM edge_event_receipts WHERE gateway_id=%s AND event_id=%s",
                    (consumer.server.gateway_id, allowed_forecast["event_id"]),
                )
                assert receipt == [("applied",)]
        # New dashboard connections recover the persisted forecast via the cache.
        async with websockets.connect(
            consumer.ticket_url("dashboard", bearer), origin="http://localhost"
        ) as dashboard:
            assert (
                await receive(dashboard, "TREND_PREDICTION_V2", consumer, "dashboard")
                == forecast
            )

    asyncio.run(run())


def test_http_consumer_rejects_a_response_incompatible_with_advertised_schema(consumer):
    _, bearer = consumer.signed_in()
    consumer.operations["current_user"]["responses"]["200"]["schema"] = (
        "v1/rest-issued-ws-ticket.schema.json"
    )
    with pytest.raises(ValidationError):
        consumer.request("current_user", bearer=bearer)


def test_http_consumer_rejects_an_error_incompatible_with_advertised_schema(consumer):
    consumer.operations["list_reservations"]["error_schema"] = (
        "v1/rest-issued-ws-ticket.schema.json"
    )
    with pytest.raises(ValidationError):
        consumer.request("list_reservations")


def test_http_consumer_validates_nonempty_collections_and_their_items(consumer):
    _, bearer = consumer.signed_in()
    assert (
        consumer.request(
            "create_reservation", payload={"spot_id": "R02-A"}, bearer=bearer
        )[0]
        == 200
    )
    admin_id = insert_user(consumer.server, role="admin")
    admin_bearer = token(consumer.server, admin_id, "admin", int(time.time()) + 60)
    status, _, raw = consumer.request("list_reservations", bearer=admin_bearer)
    assert status == 200 and json.loads(raw)[0]["spot_id"] == "R02-A"
    consumer.operations["list_reservations"]["responses"]["200"]["schema"] = (
        "v1/rest-issued-ws-ticket.schema.json"
    )
    with pytest.raises(ValidationError):
        consumer.request("list_reservations", bearer=admin_bearer)


@pytest.mark.parametrize("kind", ["SPOT_UPDATE", "RESERVATION_CONFIRMED"])
def test_websocket_consumer_checks_every_frame_against_advertised_schema(
    consumer, kind
):
    _, bearer = consumer.signed_in()
    url = consumer.ticket_url("app", bearer)
    consumer.sockets["app"]["send"]["schema"] = "v1/gateway-to-vision.schema.json"

    async def run():
        async with websockets.connect(url, origin="http://localhost") as socket:
            with pytest.raises(ValidationError):
                await receive(socket, kind, consumer, "app")

    asyncio.run(run())
