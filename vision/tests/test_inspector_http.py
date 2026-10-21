# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

import http.client
import json

import pytest

from inspection import PreviewBroker
from inspector_http import InspectorServer


@pytest.fixture
def inspector():
    broker = PreviewBroker(("camera-a", "camera-b"))
    with InspectorServer(broker, port=0) as server:
        yield server, broker


def request(server, path="/", *, method="GET", headers=None, body=None):
    connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=2)
    try:
        connection.request(method, path, headers=headers or {}, body=body)
        response = connection.getresponse()
        return response.status, dict(response.getheaders()), response.read()
    finally:
        connection.close()


def test_only_loopback_assets_and_configured_inputs_are_exposed(inspector):
    server, broker = inspector
    code, headers, body = request(server)
    assert code == 200 and b"input" in body
    assert headers["Cache-Control"] == "no-store"
    assert "default-src 'self'" in headers["Content-Security-Policy"]
    assert request(server, "/app.js")[0] == 200
    assert request(server, "/style.css")[0] == 200
    api = {"X-Inspector-Request": "1"}
    code, headers, body = request(server, "/api/status", headers=api)
    assert code == 200
    assert json.loads(body)["inputs"][0] == {"id": "camera-a", "status": "starting"}
    code, _, body = request(server, "/api/frame?input=camera-b", headers=api)
    assert code == 200 and json.loads(body)["input_id"] == "camera-b"
    assert broker.selected == "camera-b"
    assert request(server, "/api/frame?input=unknown", headers=api)[0] == 404
    assert request(server, "/../../configuration.py")[0] == 404


@pytest.mark.parametrize(
    "headers",
    [
        {"Host": "attacker.invalid"},
        {"Origin": "https://attacker.invalid"},
        {"Sec-Fetch-Site": "cross-site"},
        {"X-Inspector-Request": "0"},
        {"Transfer-Encoding": "chunked"},
        {"Content-Length": "9000"},
    ],
)
def test_cross_origin_and_body_requests_do_not_activate_preview(inspector, headers):
    server, broker = inspector
    code, response_headers, _ = request(
        server, "/api/frame?input=camera-a", headers=headers
    )
    assert code in (400, 403, 413)
    assert "Access-Control-Allow-Origin" not in response_headers
    assert broker.selected is None


def test_api_requires_custom_header_and_never_accepts_writes(inspector):
    server, broker = inspector
    assert request(server, "/api/status")[0] == 403
    assert (
        request(server, "/api/frame?input=camera-a", method="POST", body="{}")[0] == 405
    )
    assert broker.selected is None
    assert (
        request(
            server, "/api/frame?input=a&input=b", headers={"X-Inspector-Request": "1"}
        )[0]
        == 400
    )


def test_request_budget_is_bounded(inspector):
    server, _ = inspector
    codes = [request(server)[0] for _ in range(35)]
    assert 429 in codes


def test_slow_headers_cannot_hold_the_serial_server(inspector, monkeypatch):
    import socket
    import threading
    import inspector_http

    monkeypatch.setattr(inspector_http._Server, "request_timeout", 0.15, raising=False)
    server, _ = inspector
    stopped = threading.Event()
    slow = socket.create_connection(("127.0.0.1", server.port), timeout=1)
    slow.sendall(b"GET / HTTP/1.1\r\nHost: ")

    def drip():
        while not stopped.wait(0.025):
            try:
                slow.sendall(b"a")
            except OSError:
                return

    client = threading.Thread(target=drip, daemon=True)
    client.start()
    try:
        connection = http.client.HTTPConnection("127.0.0.1", server.port, timeout=0.5)
        try:
            connection.request("GET", "/")
            response = connection.getresponse()
            assert response.status == 200
            response.read()
        finally:
            connection.close()
    finally:
        stopped.set()
        slow.close()
        client.join(1)
