#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Provision installation clients and invited accounts in the Keycloak realm.

Authenticates as the realm's `provisioner` service account. Its secret, client
secrets and initial passwords travel only through private files, never argv or
output. Usernames are generated, so they carry no personal data; people sign in
with their e-mail.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import secrets
import stat
import sys
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path

USERNAME_PREFIX = "ea-"
# Crockford Base32 without the ambiguous i, l, o and u; Keycloak lowercases usernames.
USERNAME_ALPHABET = "0123456789abcdefghjkmnpqrstvwxyz"
USERNAME_LENGTH = 10  # 50 random bits
ROLES = ("user", "admin")
CLIENT_ID = re.compile(r"^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$")
RESERVED_CLIENTS = {
    "account",
    "account-console",
    "admin-cli",
    "broker",
    "hub",
    "provisioner",
    "realm-management",
    "security-admin-console",
}
INVITE_ACTIONS = ["VERIFY_EMAIL", "UPDATE_PASSWORD"]
MAX_SECRET_BYTES = 4096
TIMEOUT_SECONDS = 15


class ProvisionError(Exception):
    """A refusal or failure safe to show: it never contains a secret."""


def generate_username() -> str:
    return USERNAME_PREFIX + "".join(
        secrets.choice(USERNAME_ALPHABET) for _ in range(USERNAME_LENGTH)
    )


def read_private_file(path: Path) -> str:
    """Owner-only regular file, as the server requires for its secrets."""
    try:
        before = os.lstat(path)
        if not stat.S_ISREG(before.st_mode) or before.st_mode & 0o077:
            raise ProvisionError(f"{path} must be a private regular file")
        with open(path, encoding="utf-8") as handle:
            opened = os.fstat(handle.fileno())
            if (opened.st_dev, opened.st_ino) != (before.st_dev, before.st_ino):
                raise ProvisionError(f"{path} changed while it was opened")
            text = handle.read(MAX_SECRET_BYTES + 1)
    except OSError as error:
        raise ProvisionError(f"{path} cannot be read") from error
    if len(text) > MAX_SECRET_BYTES:
        raise ProvisionError(f"{path} is too large")
    text = text.removesuffix("\n").removesuffix("\r")
    if not text or "\n" in text:
        raise ProvisionError(f"{path} must hold one nonempty line")
    return text


def write_private_file(path: Path, text: str) -> None:
    """Creates the file with owner-only permissions; never overwrites."""
    try:
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError as error:
        raise ProvisionError(f"{path} already exists") from error
    except OSError as error:
        raise ProvisionError(f"{path} cannot be created") from error
    with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
        handle.write(text + "\n")


def base_url(value: str) -> str:
    """HTTPS origin plus optional path; plain HTTP only for this machine."""
    parsed = urllib.parse.urlsplit(value)
    local = parsed.hostname in ("localhost", "127.0.0.1", "::1")
    if (
        parsed.scheme not in ("https", "http")
        or (parsed.scheme == "http" and not local)
        or not parsed.hostname
        or parsed.query
        or parsed.fragment
        or parsed.username
        or parsed.password
    ):
        raise argparse.ArgumentTypeError(f"not an HTTPS base URL: {value!r}")
    return value.rstrip("/")


def client_id(value: str) -> str:
    if not CLIENT_ID.fullmatch(value) or value in RESERVED_CLIENTS:
        raise argparse.ArgumentTypeError(f"not an installation client ID: {value!r}")
    return value


def installation_client(client: str, api_url: str, app_url: str) -> dict:
    """Confidential, standard flow with PKCE only, exact redirect, no browser CORS."""
    return {
        "clientId": client,
        "enabled": True,
        "publicClient": False,
        "clientAuthenticatorType": "client-secret",
        "standardFlowEnabled": True,
        "implicitFlowEnabled": False,
        "directAccessGrantsEnabled": False,
        "serviceAccountsEnabled": False,
        "frontchannelLogout": False,
        "fullScopeAllowed": False,
        "rootUrl": app_url,
        "redirectUris": [f"{api_url}/auth/callback"],
        "webOrigins": [],
        # The installation receives its own roles, the username and the name only.
        "defaultClientScopes": ["basic", "profile"],
        "optionalClientScopes": [],
        "attributes": {
            "pkce.code.challenge.method": "S256",
            "backchannel.logout.url": f"{api_url}/auth/backchannel-logout",
            "backchannel.logout.session.required": "true",
            "backchannel.logout.revoke.offline.tokens": "true",
            "post.logout.redirect.uris": f"{app_url}/",
        },
        "protocolMappers": [
            {
                "name": "installation roles",
                "protocol": "openid-connect",
                "protocolMapper": "oidc-usermodel-client-role-mapper",
                "consentRequired": False,
                "config": {
                    "usermodel.clientRoleMapping.clientId": client,
                    "multivalued": "true",
                    "claim.name": "estaciona_roles",
                    "jsonType.label": "String",
                    "id.token.claim": "true",
                    "access.token.claim": "false",
                    "userinfo.token.claim": "false",
                    "introspection.token.claim": "false",
                },
            }
        ],
    }


class Keycloak:
    """Minimal Admin REST API client for one realm."""

    def __init__(self, server: str, realm: str, secret: str):
        self.server, self.realm = server, realm
        self.admin = f"{server}/admin/realms/{urllib.parse.quote(realm)}"
        token = self._send(
            "POST",
            f"{server}/realms/{urllib.parse.quote(realm)}/protocol/openid-connect/token",
            form={
                "grant_type": "client_credentials",
                "client_id": "provisioner",
                "client_secret": secret,
            },
            authenticated=False,
        )
        self.token = token["access_token"]

    def _send(self, method, url, *, body=None, form=None, authenticated=True):
        headers = {"Accept": "application/json"}
        data = None
        if authenticated:
            headers["Authorization"] = f"Bearer {self.token}"
        if form is not None:
            data = urllib.parse.urlencode(form).encode()
            headers["Content-Type"] = "application/x-www-form-urlencoded"
        elif body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(request, timeout=TIMEOUT_SECONDS) as response:
                payload = response.read()
                location = response.headers.get("Location")
        except urllib.error.HTTPError as error:
            raise KeycloakStatus(error.code, _error_message(error)) from None
        except OSError as error:
            raise ProvisionError(
                f"Keycloak unreachable: {error.__class__.__name__}"
            ) from None
        if location and not payload:
            return {"id": location.rstrip("/").rsplit("/", 1)[-1]}
        return json.loads(payload) if payload else None

    def call(self, method, path, **kwargs):
        return self._send(method, self.admin + path, **kwargs)

    def client_uuid(self, client: str) -> str:
        found = self.call(
            "GET", "/clients?" + urllib.parse.urlencode({"clientId": client})
        )
        if not found:
            raise ProvisionError(f"installation client {client!r} does not exist")
        return found[0]["id"]

    def user_by_email(self, email: str):
        query = urllib.parse.urlencode({"email": email, "exact": "true"})
        found = self.call("GET", f"/users?{query}")
        return found[0] if found else None

    def roles(self, client_uuid: str, names) -> list:
        return [
            self.call("GET", f"/clients/{client_uuid}/roles/{name}") for name in names
        ]


class KeycloakStatus(ProvisionError):
    def __init__(self, status: int, message: str):
        super().__init__(f"Keycloak answered {status}: {message}")
        self.status, self.message = status, message


def _error_message(error: urllib.error.HTTPError) -> str:
    try:
        body = json.loads(error.read() or b"{}")
    except ValueError:
        return "unexpected response"
    text = (
        body.get("errorMessage") or body.get("error_description") or body.get("error")
    )
    return str(text)[:200] if text else "no detail"


def require_new_file(path: Path) -> None:
    if path.exists() or path.is_symlink():
        raise ProvisionError(f"{path} already exists")


def create_installation(kc: Keycloak, args) -> dict:
    require_new_file(args.secret_out)
    representation = installation_client(args.client_id, args.api_url, args.app_url)
    try:
        uuid = kc.call("POST", "/clients", body=representation)["id"]
    except KeycloakStatus as error:
        if error.status == 409:
            raise ProvisionError(
                f"installation client {args.client_id!r} already exists"
            ) from None
        raise
    try:
        for name in ROLES:
            kc.call("POST", f"/clients/{uuid}/roles", body={"name": name})
        # With fullScopeAllowed off, only roles in the client's scope reach its tokens.
        kc.call(
            "POST",
            f"/clients/{uuid}/scope-mappings/clients/{uuid}",
            body=kc.roles(uuid, ROLES),
        )
        secret = kc.call("GET", f"/clients/{uuid}/client-secret")["value"]
        write_private_file(args.secret_out, secret)
    except ProvisionError:
        kc.call("DELETE", f"/clients/{uuid}")
        raise
    return {"installation_client": args.client_id, "secret_file": str(args.secret_out)}


def rotate_installation_secret(kc: Keycloak, args) -> dict:
    require_new_file(args.secret_out)
    uuid = kc.client_uuid(args.client_id)
    rotated = kc.call("POST", f"/clients/{uuid}/client-secret")
    write_private_file(args.secret_out, rotated["value"])
    return {"installation_client": args.client_id, "secret_file": str(args.secret_out)}


def invite_user(kc: Keycloak, args) -> dict:
    uuid = kc.client_uuid(args.installation)
    password = (
        read_private_file(args.temporary_password_file)
        if args.temporary_password_file
        else None
    )
    representation = {
        "email": args.email,
        "enabled": True,
        "emailVerified": False,
        "requiredActions": INVITE_ACTIONS,
    }
    if args.first_name:
        representation["firstName"] = args.first_name
    if args.last_name:
        representation["lastName"] = args.last_name
    for _ in range(5):
        username = generate_username()
        try:
            user = kc.call(
                "POST", "/users", body={**representation, "username": username}
            )
            break
        except KeycloakStatus as error:
            if error.status != 409:
                raise
            if "email" in error.message.lower():
                raise ProvisionError(
                    "an account with this e-mail exists; use user-grant to add access"
                ) from None
    else:
        raise ProvisionError("could not generate a free username")
    # A failed invitation leaves no account behind, so it can simply be retried.
    try:
        kc.call(
            "POST",
            f"/users/{user['id']}/role-mappings/clients/{uuid}",
            body=kc.roles(uuid, [args.role]),
        )
        if password is not None:
            kc.call(
                "PUT",
                f"/users/{user['id']}/reset-password",
                body={"type": "password", "value": password, "temporary": True},
            )
        if not args.no_email:
            kc.call(
                "PUT", f"/users/{user['id']}/execute-actions-email", body=INVITE_ACTIONS
            )
    except ProvisionError:
        kc.call("DELETE", f"/users/{user['id']}")
        raise
    return {
        "username": username,
        "user_id": user["id"],
        "installation_client": args.installation,
        "role": args.role,
        "invitation_sent": not args.no_email,
    }


def grant_access(kc: Keycloak, args) -> dict:
    uuid = kc.client_uuid(args.installation)
    user = kc.user_by_email(args.email)
    if user is None:
        raise ProvisionError("no account with this e-mail; use user-invite")
    kc.call(
        "POST",
        f"/users/{user['id']}/role-mappings/clients/{uuid}",
        body=kc.roles(uuid, [args.role]),
    )
    return {
        "username": user["username"],
        "installation_client": args.installation,
        "role": args.role,
    }


def revoke_access(kc: Keycloak, args) -> dict:
    """Removing a role only applies at the next sign-in, so sessions end too:
    Keycloak then sends back-channel logout to every installation in use."""
    uuid = kc.client_uuid(args.installation)
    user = kc.user_by_email(args.email)
    if user is None:
        raise ProvisionError("no account with this e-mail")
    kc.call(
        "DELETE",
        f"/users/{user['id']}/role-mappings/clients/{uuid}",
        body=kc.roles(uuid, args.roles),
    )
    kc.call("POST", f"/users/{user['id']}/logout")
    return {
        "username": user["username"],
        "installation_client": args.installation,
        "revoked": args.roles,
        "sessions_ended": True,
    }


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    root.add_argument(
        "--server",
        type=base_url,
        required=True,
        help="Keycloak base URL reachable from here",
    )
    root.add_argument("--realm", default="estaciona")
    root.add_argument("--provisioner-secret-file", type=Path, required=True)
    commands = root.add_subparsers(dest="command", required=True)

    create = commands.add_parser(
        "installation-create", help="register an installation client"
    )
    create.add_argument("--client-id", type=client_id, required=True)
    create.add_argument(
        "--api-url", type=base_url, required=True, help="installation server base URL"
    )
    create.add_argument(
        "--app-url", type=base_url, required=True, help="installation web app base URL"
    )
    create.add_argument(
        "--secret-out",
        type=Path,
        required=True,
        help="new private file for OIDC_CLIENT_SECRET_FILE",
    )
    create.set_defaults(run=create_installation)

    rotate = commands.add_parser(
        "installation-rotate-secret", help="issue a new client secret"
    )
    rotate.add_argument("--client-id", type=client_id, required=True)
    rotate.add_argument("--secret-out", type=Path, required=True)
    rotate.set_defaults(run=rotate_installation_secret)

    invite = commands.add_parser(
        "user-invite", help="create an account with a generated username"
    )
    invite.add_argument("--email", required=True)
    invite.add_argument("--installation", type=client_id, required=True)
    invite.add_argument("--role", choices=ROLES, default="user")
    invite.add_argument("--first-name")
    invite.add_argument("--last-name")
    invite.add_argument(
        "--temporary-password-file",
        type=Path,
        help="private file; changed at first sign-in",
    )
    invite.add_argument(
        "--no-email", action="store_true", help="do not send the invitation e-mail"
    )
    invite.set_defaults(run=invite_user)

    grant = commands.add_parser(
        "user-grant", help="give an existing account access to an installation"
    )
    grant.add_argument("--email", required=True)
    grant.add_argument("--installation", type=client_id, required=True)
    grant.add_argument("--role", choices=ROLES, default="user")
    grant.set_defaults(run=grant_access)

    revoke = commands.add_parser(
        "user-revoke", help="remove access and end the account's sessions"
    )
    revoke.add_argument("--email", required=True)
    revoke.add_argument("--installation", type=client_id, required=True)
    revoke.add_argument(
        "--role",
        dest="roles",
        action="append",
        choices=ROLES,
        help="default: all roles",
    )
    revoke.set_defaults(run=revoke_access)
    return root


def main(argv=None) -> int:
    args = parser().parse_args(argv)
    if getattr(args, "roles", ()) is None:
        args.roles = list(ROLES)
    try:
        kc = Keycloak(
            args.server, args.realm, read_private_file(args.provisioner_secret_file)
        )
        print(json.dumps(args.run(kc, args)))
    except ProvisionError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
