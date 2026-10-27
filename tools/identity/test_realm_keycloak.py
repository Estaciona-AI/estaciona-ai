# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Browser sign-in against a real Keycloak that imported realm/estaciona.json.

Opt-in: start the pinned image from infra/identity/compose.yaml with
--import-realm, HUB_REDIRECT_URI=http://localhost/entrar/ and a bootstrap
administrator, then set KEYCLOAK_TEST_URL (e.g. http://127.0.0.1:18081),
KEYCLOAK_TEST_ADMIN and KEYCLOAK_TEST_ADMIN_PASSWORD. It creates two accounts.
"""

import base64
import hashlib
import http.cookiejar
import json
import os
import re
import unittest
import urllib.error
import urllib.parse
import urllib.request
import uuid

BASE = os.environ.get("KEYCLOAK_TEST_URL", "")
PASSWORD = "Correct-horse-9"


@unittest.skipUnless(BASE, "set KEYCLOAK_TEST_URL to a disposable Keycloak")
class RealmSignInTest(unittest.TestCase):
    def setUp(self):
        form = urllib.parse.urlencode(
            {
                "grant_type": "password",
                "client_id": "admin-cli",
                "username": os.environ["KEYCLOAK_TEST_ADMIN"],
                "password": os.environ["KEYCLOAK_TEST_ADMIN_PASSWORD"],
            }
        ).encode()
        url = f"{BASE}/realms/master/protocol/openid-connect/token"
        self.token = json.load(urllib.request.urlopen(url, form))["access_token"]

    def admin(self, method, path, body=None):
        request = urllib.request.Request(
            f"{BASE}/admin/realms/estaciona{path}",
            method=method,
            data=None if body is None else json.dumps(body).encode(),
            headers={
                "Authorization": f"Bearer {self.token}",
                "Content-Type": "application/json",
            },
        )
        with urllib.request.urlopen(request) as response:
            raw = response.read()
            return response.headers.get("Location"), json.loads(raw) if raw else None

    def account(self, mfa):
        name = f"t{uuid.uuid4().hex[:10]}"
        location, _ = self.admin(
            "POST",
            "/users",
            {
                "username": name,
                "email": f"{name}@example.invalid",
                "enabled": True,
                "emailVerified": True,
                "firstName": "Teste",
                "lastName": "Real",
                "credentials": [
                    {"type": "password", "value": PASSWORD, "temporary": False}
                ],
            },
        )
        if mfa:
            role = self.admin("GET", "/roles/estaciona-mfa")[1]
            self.admin(
                "POST",
                f"/users/{location.rsplit('/', 1)[1]}/role-mappings/realm",
                [role],
            )
        return name

    def sign_in(self, name):
        """Where the browser goes right after submitting the password."""
        # Keycloak marks its cookies Secure; this disposable server is plain HTTP.
        policy = http.cookiejar.DefaultCookiePolicy(secure_protocols=("https", "http"))

        class Stop(urllib.request.HTTPRedirectHandler):
            def redirect_request(self, *args):
                return None

        opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar(policy)), Stop
        )
        verifier = base64.urlsafe_b64encode(os.urandom(32)).rstrip(b"=").decode()
        challenge = (
            base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest())
            .rstrip(b"=")
            .decode()
        )
        query = urllib.parse.urlencode(
            {
                "client_id": "hub",
                "response_type": "code",
                "scope": "openid",
                "redirect_uri": "http://localhost/entrar/",
                "code_challenge": challenge,
                "code_challenge_method": "S256",
                "state": "state",
                "nonce": "nonce",
            }
        )
        page = opener.open(
            f"{BASE}/realms/estaciona/protocol/openid-connect/auth?{query}"
        ).read()
        action = re.search(rb'action="([^"]+)"', page)[1].decode().replace("&amp;", "&")
        credentials = urllib.parse.urlencode({"username": name, "password": PASSWORD})
        with self.assertRaises(urllib.error.HTTPError) as redirect:
            opener.open(action, credentials.encode())
        self.assertEqual(redirect.exception.code, 302)
        return redirect.exception.headers["Location"]

    def test_the_realm_uses_its_own_browser_flow(self):
        realm = self.admin("GET", "")[1]
        self.assertEqual(realm["browserFlow"], "estaciona browser")

    def test_a_regular_account_signs_in_with_a_password(self):
        location = self.sign_in(self.account(mfa=False))
        self.assertTrue(location.startswith("http://localhost/entrar/?"), location)
        self.assertIn("code=", location)

    def test_an_administrator_sets_up_a_one_time_code_before_any_code_is_issued(self):
        location = self.sign_in(self.account(mfa=True))
        self.assertIn("execution=CONFIGURE_TOTP", location)
        self.assertNotIn("code=", location)


if __name__ == "__main__":
    unittest.main()
