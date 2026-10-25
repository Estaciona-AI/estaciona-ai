# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Provisioning against an in-process fake of the Keycloak Admin REST API."""

import contextlib
import io
import json
import os
import re
import stat
import tempfile
import threading
import unittest
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import provision

PROVISIONER_SECRET = "provisioner-secret-not-for-output"
CLIENT_SECRET = "client-secret-not-for-output"
PASSWORD = "Temporary-Pass-not-for-output"
ADMIN = "/admin/realms/estaciona"


class FakeKeycloak:
    def __init__(self):
        self.requests = []
        self.clients = {"hub-uuid": {"clientId": "hub"}}
        self.users = {}
        self.username_conflicts = 0
        self.fail = set()
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), self.handler())
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    def handler(self):
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def _respond(self, status, body=None, location=None):
                payload = b"" if body is None else json.dumps(body).encode()
                self.send_response(status)
                if location:
                    self.send_header("Location", location)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def _handle(self):
                length = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(length) if length else b""
                url = urllib.parse.urlsplit(self.path)
                body = None
                if raw and self.headers.get("Content-Type", "").startswith(
                    "application/json"
                ):
                    body = json.loads(raw)
                fake.requests.append((self.command, url.path, url.query, body, raw))
                status, payload, location = fake.route(
                    self.command,
                    url.path,
                    urllib.parse.parse_qs(url.query),
                    body,
                    raw,
                    self.headers.get("Authorization"),
                )
                self._respond(status, payload, location)

            do_GET = do_POST = do_PUT = do_DELETE = _handle

        return Handler

    def route(self, method, path, query, body, raw, authorization):
        if path == "/realms/estaciona/protocol/openid-connect/token":
            form = urllib.parse.parse_qs(raw.decode())
            if form.get("client_secret") != [PROVISIONER_SECRET]:
                return 401, {"error": "unauthorized_client"}, None
            return 200, {"access_token": "fake-token"}, None
        if authorization != "Bearer fake-token":
            return 401, None, None
        if not path.startswith(ADMIN):
            return 404, None, None
        path = path[len(ADMIN) :]
        if (method, path) in self.fail:
            return 500, {"errorMessage": "injected failure"}, None
        if method == "POST" and path == "/clients":
            if any(c["clientId"] == body["clientId"] for c in self.clients.values()):
                return 409, {"errorMessage": "Client exists"}, None
            uuid = f"client-{len(self.clients)}"
            self.clients[uuid] = body
            return 201, None, f"{self.url}{ADMIN}/clients/{uuid}"
        if method == "GET" and path == "/clients":
            wanted = query.get("clientId", [None])[0]
            return (
                200,
                [{"id": u} for u, c in self.clients.items() if c["clientId"] == wanted],
                None,
            )
        match = re.fullmatch(r"/clients/([^/]+)/roles/([^/]+)", path)
        if method == "GET" and match:
            return 200, {"id": f"role-{match[2]}", "name": match[2]}, None
        if re.fullmatch(r"/clients/[^/]+/client-secret", path):
            return 200, {"type": "secret", "value": CLIENT_SECRET}, None
        if method == "POST" and path == "/users":
            if self.username_conflicts:
                self.username_conflicts -= 1
                return 409, {"errorMessage": "User exists with same username"}, None
            if any(u["email"] == body["email"] for u in self.users.values()):
                return 409, {"errorMessage": "User exists with same email"}, None
            uid = f"user-{len(self.users)}"
            self.users[uid] = body
            return 201, None, f"{self.url}{ADMIN}/users/{uid}"
        if method == "GET" and path == "/users":
            email = query.get("email", [None])[0]
            return (
                200,
                [
                    {"id": i, "username": u["username"]}
                    for i, u in self.users.items()
                    if u["email"] == email
                ],
                None,
            )
        if method == "DELETE" and path.startswith("/users/") and path.count("/") == 2:
            self.users.pop(path.rsplit("/", 1)[1], None)
            return 204, None, None
        if method == "DELETE" and path.startswith("/clients/") and path.count("/") == 2:
            self.clients.pop(path.rsplit("/", 1)[1], None)
            return 204, None, None
        if method == "POST" and path.endswith("/roles"):
            return 201, None, f"{self.url}{ADMIN}{path}/{body['name']}"
        return 204, None, None


def private(directory, name, text, mode=0o600):
    path = Path(directory) / name
    path.write_text(text)
    path.chmod(mode)
    return path


class ProvisionTest(unittest.TestCase):
    def setUp(self):
        self.fake = FakeKeycloak()
        self.addCleanup(self.fake.close)
        self.directory = tempfile.mkdtemp()
        self.secret = private(self.directory, "provisioner", PROVISIONER_SECRET + "\n")

    def run_tool(self, *args):
        output, errors = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(errors):
            code = provision.main(
                [
                    "--server",
                    self.fake.url,
                    "--provisioner-secret-file",
                    str(self.secret),
                    *map(str, args),
                ]
            )
        for secret in (PROVISIONER_SECRET, CLIENT_SECRET, PASSWORD):
            self.assertNotIn(secret, output.getvalue() + errors.getvalue())
        return code, output.getvalue(), errors.getvalue()

    def create(self, out):
        return self.run_tool(
            "installation-create",
            "--client-id",
            "uniube-centro",
            "--api-url",
            "http://localhost:8000/",
            "--app-url",
            "http://localhost:5173",
            "--secret-out",
            out,
        )

    def test_generated_usernames_are_random_and_carry_no_personal_data(self):
        names = {provision.generate_username() for _ in range(2000)}
        self.assertEqual(len(names), 2000)
        for name in names:
            self.assertRegex(name, r"^ea-[0-9abcdefghjkmnpqrstvwxyz]{10}$")

    def test_installation_client_allows_only_the_code_flow_to_its_callback(self):
        out = Path(self.directory) / "client.secret"
        code, output, _ = self.create(out)
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(output)["installation_client"], "uniube-centro")
        (client,) = [c for c in self.fake.clients.values() if c["clientId"] != "hub"]
        self.assertFalse(client["publicClient"])
        self.assertTrue(client["standardFlowEnabled"])
        self.assertFalse(client["implicitFlowEnabled"])
        self.assertFalse(client["directAccessGrantsEnabled"])
        self.assertFalse(client["serviceAccountsEnabled"])
        self.assertFalse(client["fullScopeAllowed"])
        self.assertEqual(
            client["redirectUris"], ["http://localhost:8000/auth/callback"]
        )
        self.assertEqual(client["webOrigins"], [])
        self.assertNotIn("email", client["defaultClientScopes"])
        self.assertEqual(client["optionalClientScopes"], [])
        attributes = client["attributes"]
        self.assertEqual(attributes["pkce.code.challenge.method"], "S256")
        self.assertEqual(
            attributes["backchannel.logout.url"],
            "http://localhost:8000/auth/backchannel-logout",
        )
        (mapper,) = client["protocolMappers"]
        self.assertEqual(mapper["config"]["claim.name"], "estaciona_roles")
        self.assertEqual(
            mapper["config"]["usermodel.clientRoleMapping.clientId"], "uniube-centro"
        )
        self.assertEqual(out.read_text(), CLIENT_SECRET + "\n")
        self.assertEqual(stat.S_IMODE(os.stat(out).st_mode), 0o600)
        scope = {r[1]: r[3] for r in self.fake.requests if "scope-mappings" in r[1]}
        own = f"{ADMIN}/clients/client-1/scope-mappings/clients/client-1"
        hub = f"{ADMIN}/clients/hub-uuid/scope-mappings/clients/client-1"
        for path in (own, hub):
            self.assertEqual([role["name"] for role in scope[path]], ["user", "admin"])

    def test_an_existing_secret_file_is_never_overwritten_or_reached(self):
        out = private(self.directory, "client.secret", "previous")
        code, _, errors = self.create(out)
        self.assertEqual(code, 1)
        self.assertIn("already exists", errors)
        self.assertEqual(out.read_text(), "previous")
        self.assertEqual(list(self.fake.clients), ["hub-uuid"])

    def test_a_failed_registration_removes_the_new_client(self):
        self.fake.fail.add(
            ("POST", "/clients/hub-uuid/scope-mappings/clients/client-1")
        )
        out = Path(self.directory) / "client.secret"
        code, _, _ = self.create(out)
        self.assertEqual(code, 1)
        self.assertEqual(list(self.fake.clients), ["hub-uuid"])
        self.assertFalse(out.exists())

    def invite(self, *extra):
        return self.run_tool(
            "user-invite",
            "--email",
            "pessoa@example.invalid",
            "--installation",
            "uniube-centro",
            *extra,
        )

    def test_invitation_generates_the_username_and_sets_a_temporary_password(self):
        self.create(Path(self.directory) / "client.secret")
        self.fake.username_conflicts = 2
        password = private(self.directory, "password", PASSWORD + "\n")
        code, output, _ = self.invite(
            "--role", "admin", "--temporary-password-file", password, "--no-email"
        )
        self.assertEqual(code, 0)
        result = json.loads(output)
        self.assertRegex(result["username"], r"^ea-[0-9a-z]{10}$")
        (user,) = self.fake.users.values()
        self.assertEqual(user["username"], result["username"])
        self.assertEqual(user["requiredActions"], ["VERIFY_EMAIL", "UPDATE_PASSWORD"])
        self.assertFalse(user["emailVerified"])
        reset = [r for r in self.fake.requests if r[1].endswith("/reset-password")]
        self.assertEqual(
            reset[0][3], {"type": "password", "value": PASSWORD, "temporary": True}
        )
        mapping = [r for r in self.fake.requests if "role-mappings" in r[1]]
        self.assertEqual(mapping[0][3][0]["name"], "admin")
        self.assertFalse(
            any("execute-actions-email" in r[1] for r in self.fake.requests)
        )

    def test_a_known_email_is_not_invited_twice(self):
        self.create(Path(self.directory) / "client.secret")
        self.assertEqual(self.invite("--no-email")[0], 0)
        code, _, errors = self.invite("--no-email")
        self.assertEqual(code, 1)
        self.assertIn("user-grant", errors)
        self.assertEqual(len(self.fake.users), 1)

    def test_a_failed_invitation_leaves_no_account(self):
        self.create(Path(self.directory) / "client.secret")
        self.fake.fail.add(("PUT", "/users/user-0/execute-actions-email"))
        code, _, errors = self.invite()
        self.assertEqual(code, 1)
        self.assertIn("injected failure", errors)
        self.assertEqual(self.fake.users, {})

    def test_revocation_removes_roles_and_ends_every_session(self):
        self.create(Path(self.directory) / "client.secret")
        self.invite("--no-email")
        code, output, _ = self.run_tool(
            "user-revoke",
            "--email",
            "pessoa@example.invalid",
            "--installation",
            "uniube-centro",
        )
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(output)["revoked"], ["user", "admin"])
        removed = [
            r
            for r in self.fake.requests
            if r[0] == "DELETE" and "role-mappings" in r[1]
        ]
        self.assertEqual([role["name"] for role in removed[0][3]], ["user", "admin"])
        self.assertTrue(
            any(r[0] == "POST" and r[1].endswith("/logout") for r in self.fake.requests)
        )

    def test_shared_secret_files_and_unsafe_arguments_are_refused(self):
        self.secret.chmod(0o644)
        code, _, errors = self.create(Path(self.directory) / "client.secret")
        self.assertEqual(code, 1)
        self.assertIn("private regular file", errors)
        self.assertEqual(self.fake.requests, [])
        for value in ("http://auth.example.com", "https://a.example/?x=1", "ftp://a"):
            with self.assertRaises(Exception):
                provision.base_url(value)
        self.assertEqual(
            provision.base_url("https://a.example/x/"), "https://a.example/x"
        )
        for value in ("hub", "provisioner", "Uniube", "-x", "a" * 64):
            with self.assertRaises(Exception):
                provision.client_id(value)


if __name__ == "__main__":
    unittest.main()
