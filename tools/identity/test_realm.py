# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Security invariants of the realm imported by infra/identity."""

import json
import unittest
from pathlib import Path

REALM = Path(__file__).parents[2] / "infra" / "identity" / "realm" / "estaciona.json"


class RealmTest(unittest.TestCase):
    def setUp(self):
        self.realm = json.loads(REALM.read_text())
        self.clients = {c["clientId"]: c for c in self.realm["clients"]}

    def test_accounts_are_provisioned_and_protected_against_guessing(self):
        realm = self.realm
        self.assertFalse(realm["registrationAllowed"])
        self.assertFalse(realm["editUsernameAllowed"])
        self.assertTrue(realm["loginWithEmailAllowed"])
        self.assertFalse(realm["duplicateEmailsAllowed"])
        self.assertTrue(realm["verifyEmail"])
        self.assertTrue(realm["bruteForceProtected"])
        self.assertLessEqual(realm["failureFactor"], 5)
        policy = realm["passwordPolicy"]
        for rule in ("length(12)", "notUsername", "notEmail", "hashAlgorithm(argon2)"):
            self.assertIn(rule, policy)
        self.assertEqual(realm["sslRequired"], "external")
        self.assertTrue(realm["eventsEnabled"] and realm["adminEventsEnabled"])

    def test_tokens_are_signed_only_with_ed25519(self):
        self.assertEqual(self.realm["defaultSignatureAlgorithm"], "EdDSA")
        providers = self.realm["components"]["org.keycloak.keys.KeyProvider"]
        self.assertEqual([p["providerId"] for p in providers], ["eddsa-generated"])
        self.assertEqual(providers[0]["config"]["eddsaEllipticCurveKey"], ["Ed25519"])

    def test_no_client_allows_the_implicit_or_password_flows(self):
        for client in self.realm["clients"]:
            self.assertFalse(client["implicitFlowEnabled"], client["clientId"])
            self.assertFalse(client["directAccessGrantsEnabled"], client["clientId"])

    def test_hub_is_a_public_pkce_client_with_exact_redirects(self):
        hub = self.clients["hub"]
        self.assertTrue(hub["publicClient"])
        self.assertEqual(hub["attributes"]["pkce.code.challenge.method"], "S256")
        self.assertEqual(hub["redirectUris"], ["${HUB_REDIRECT_URI}"])
        self.assertFalse(hub["serviceAccountsEnabled"])
        (mapper,) = hub["protocolMappers"]
        self.assertEqual(mapper["config"]["id.token.claim"], "true")
        self.assertEqual(mapper["config"]["access.token.claim"], "false")

    def test_provisioner_holds_only_client_and_user_management(self):
        provisioner = self.clients["provisioner"]
        self.assertFalse(provisioner["standardFlowEnabled"])
        self.assertTrue(provisioner["serviceAccountsEnabled"])
        self.assertEqual(provisioner["secret"], "${PROVISIONER_CLIENT_SECRET}")
        (account,) = self.realm["users"]
        self.assertEqual(account["serviceAccountClientId"], "provisioner")
        self.assertEqual(set(account["clientRoles"]), {"realm-management"})
        roles = set(account["clientRoles"]["realm-management"])
        self.assertEqual(
            roles,
            {
                "manage-clients",
                "view-clients",
                "query-clients",
                "manage-users",
                "view-users",
                "query-users",
            },
        )
        self.assertNotIn("realm-admin", roles)
        self.assertNotIn("realmRoles", account)


if __name__ == "__main__":
    unittest.main()
