// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

//! Discovery metadata only. Handlers remain the authority for authorization.
use serde_json::{Value, json};

fn body(encoding: &str, schema: Option<&str>) -> Value {
    json!({"encoding": encoding, "schema": schema})
}

fn rest(
    id: &str,
    method: &str,
    path: &str,
    authentication: &str,
    authorization: &str,
    request_schema: Option<&str>,
    response: (u16, Value),
) -> Value {
    let (status, response) = response;
    json!({
        "id": id, "method": method,
        "path": format!("/api/v1{path}"), "legacy_path": path,
        "authentication": authentication, "authorization": authorization,
        "availability": "always",
        "request_schema": request_schema,
        "request_encoding": if request_schema.is_some() {"json"} else {"none"},
        "responses": {status.to_string(): response},
        "error_schema": "v1/rest-api-error.schema.json"
    })
}

pub(super) fn document() -> Value {
    let json_body = |schema| body("json", Some(schema));
    let mut operations = vec![
        rest(
            "current_user",
            "GET",
            "/me",
            "human_session",
            "authenticated_user",
            None,
            (200, json_body("v1/rest-user-response.schema.json")),
        ),
        rest(
            "update_eligibility",
            "PUT",
            "/me/eligibility",
            "human_session",
            "authenticated_user",
            Some("v1/rest-update-eligibility.schema.json"),
            (200, json_body("v1/rest-user-response.schema.json")),
        ),
        rest(
            "export_personal_data",
            "GET",
            "/me/export",
            "human_session",
            "authenticated_user",
            None,
            (200, json_body("v1/rest-personal-data-export.schema.json")),
        ),
        rest(
            "delete_account",
            "DELETE",
            "/me",
            "human_session",
            "authenticated_user",
            None,
            (204, body("none", None)),
        ),
        rest(
            "get_user",
            "GET",
            "/users/{id}",
            "human_session",
            "self_or_admin",
            None,
            (200, json_body("v1/rest-user-response.schema.json")),
        ),
        rest(
            "logout",
            "POST",
            "/auth/logout",
            "human_session",
            "current_session",
            None,
            (200, json_body("v1/rest-logout-response.schema.json")),
        ),
        rest(
            "websocket_ticket",
            "POST",
            "/auth/ws-ticket",
            "human_session",
            "dashboard_requires_admin",
            Some("v1/rest-issue-ws-ticket.schema.json"),
            (201, json_body("v1/rest-issued-ws-ticket.schema.json")),
        ),
        rest(
            "create_reservation",
            "POST",
            "/reservations",
            "human_session",
            "authenticated_user",
            Some("v1/rest-create-reservation.schema.json"),
            (200, json_body("v1/rest-reservation-response.schema.json")),
        ),
        rest(
            "list_reservations",
            "GET",
            "/reservations",
            "human_session",
            "admin",
            None,
            (
                200,
                json!({"encoding":"json", "schema":"v1/rest-reservation-response.schema.json", "collection":true}),
            ),
        ),
        rest(
            "cancel_reservation",
            "PUT",
            "/reservations/{id}/cancel",
            "human_session",
            "reservation_owner",
            None,
            (200, body("text", None)),
        ),
        rest(
            "confirm_occupancy",
            "POST",
            "/reservations/{id}/confirm",
            "human_session",
            "reservation_owner",
            None,
            (200, body("text", None)),
        ),
        rest(
            "extend_reservation",
            "PUT",
            "/reservations/{id}/extend",
            "human_session",
            "reservation_owner",
            None,
            (200, body("text", None)),
        ),
        rest(
            "recommend_spot",
            "GET",
            "/reservations/recommend",
            "human_session",
            "query_user_must_match_session",
            None,
            (200, body("json", None)),
        ),
        rest(
            "update_spot_status",
            "PUT",
            "/spots/{id}/status",
            "human_session",
            "admin",
            Some("v1/rest-update-spot-status.schema.json"),
            (200, body("empty", None)),
        ),
        rest(
            "save_config",
            "POST",
            "/config",
            "human_session",
            "admin",
            None,
            (200, body("text", None)),
        ),
    ];
    // Some legacy handlers return dynamic JSON or text and have no DTO schema.
    // Explicit null references avoid advertising unchecked schemas as guarantees.
    for (id, method, path, status) in [
        (
            "list_vehicle_bindings",
            "GET",
            "/admin/vehicle-bindings",
            200,
        ),
        (
            "create_vehicle_binding",
            "POST",
            "/admin/vehicle-bindings",
            201,
        ),
        (
            "approve_vehicle_binding",
            "POST",
            "/admin/vehicle-bindings/{id}/approve",
            200,
        ),
        (
            "revoke_vehicle_binding",
            "POST",
            "/admin/vehicle-bindings/{id}/revoke",
            200,
        ),
    ] {
        let mut operation = rest(
            id,
            method,
            path,
            "human_session",
            "admin",
            None,
            (status, body("json", None)),
        );
        operation["availability"] = json!("lpr_enabled");
        if method == "POST" {
            operation["request_encoding"] = json!("json");
        }
        operations.push(operation);
    }
    for (id, path, response) in [
        (
            "capabilities",
            "/api/v1/capabilities",
            json_body("v1/rest-capabilities.schema.json"),
        ),
        (
            "contract_artifact",
            "/api/v1/contracts/{path}",
            body("json", None),
        ),
        ("health", "/health", body("text", None)),
        ("readiness", "/ready", body("text", None)),
    ] {
        operations.push(json!({
            "id":id, "method":"GET", "path":path, "legacy_path":null,
            "authentication":"public", "authorization":"public", "availability":"always",
            "request_schema":null, "request_encoding":"none", "responses":{"200":response},
            "error_schema": if matches!(id, "health" | "readiness") {None} else {Some("v1/rest-api-error.schema.json")}
        }));
    }
    for operation in &mut operations {
        match operation["id"].as_str().expect("static operation id") {
            "recommend_spot" => {
                operation["query"] = json!({"user_id":"uuid_required_matches_session"})
            }
            "save_config" => {
                operation["effect"] = json!("persist_geometry_for_next_restart");
                operation["request_encoding"] = json!("json");
            }
            "delete_account" => {
                operation["effect"] = json!("release_reservations_and_delete_local_account")
            }
            "readiness" => {
                operation["responses"]["503"] = body("text", None);
            }
            "websocket_ticket" => {
                operation["effect"] = json!("single_use_destination_bound_ticket")
            }
            "confirm_occupancy" => {
                operation["precondition"] =
                    json!("unexpired_reservation_and_observed_occupied_spot")
            }
            "create_reservation" => {
                operation["expiration_source"] = json!("server_reservation_policy")
            }
            _ => {}
        }
    }
    json!({
        "version":super::VERSION,
        "scope":"single_installation_per_process_and_database",
        "schema_null_meaning":"no_exported_dto_schema",
        "authentication":{
            "public":{"credential":null},
            "human_session":{"credential":"bearer_or_host_only_session_cookie", "validation":"registered_current_database_session", "cookie_writes":"require_allowed_origin"},
            "single_use_ticket":{"credential":"ticket_query_parameter", "issued_by":"websocket_ticket", "validation":"destination_session_and_origin"},
            "individual_gateway_bearer":{"credential":"authorization_bearer", "validation":"active_gateway_and_explicit_grants"},
            "local_shared_key":{"credential":"authorization_bearer", "validation":"gateway_process_key"}
        },
        "sign_in":{
            "protocol":"openid_connect_authorization_code_with_pkce",
            "identity_provider":"central_keycloak_realm",
            "start":"/auth/login", "callback":"/auth/callback",
            "backchannel_logout":"/auth/backchannel-logout",
            "session":"host_only_http_only_cookie",
            "accounts":"provisioned_at_identity_provider", "local_credentials":false
        },
        "installation_configuration":{
            "source":"operator_package", "public_endpoint":null,
            "files":["installation.json","config.json","spots_3d.json","reservation_policy.json"],
            "optional_files":["reservation_policy.json"],
            "secrets_source":"process_environment_or_private_files", "scope_from_client_payload":false
        },
        "http":operations,
        "websockets":[
            {"id":"app", "path":"/ws/app", "authentication":"single_use_ticket", "authorization":"authenticated_user", "ticket_destination":"app", "receive":{"schema":"v1/app-to-server.schema.json"}, "send":{"schema":"v1/server-to-app.schema.json"}, "initial_state":"active_spots", "delivery":"best_effort_disconnect_requires_resnapshot"},
            {"id":"dashboard", "path":"/ws/dashboard", "authentication":"single_use_ticket", "authorization":"admin", "ticket_destination":"dashboard", "receive":null, "send":{"schema":"v1/server-to-app.schema.json"}, "initial_state":"active_spots_and_cached_forecast_if_available", "delivery":"best_effort_disconnect_requires_resnapshot"},
            {"id":"edge", "path":"/ws/edge", "authentication":"individual_gateway_bearer", "authorization":"explicit_gateway_grants", "receive":{"schema":"v1/gateway-to-server.schema.json"}, "send":{"schema":"v1/server-to-gateway.schema.json"}, "payload":{"schema":"v1/edge-events.schema.json"}, "legacy_unwrapped_events_accepted":true, "legacy_events_have_durable_ack":false, "ack_boundary":"postgres_commit", "event_authorization":{"SPOT_UPDATE":"camera_spot_pair_and_operation", "CAR_DETECTED":"lpr_enabled_camera_operation_and_granted_spots", "PATH_UPDATE":"installation_wide_explicit_permission", "TREND_PREDICTION":"installation_wide_explicit_permission", "TREND_PREDICTION_V2":"installation_wide_explicit_permission"}}
        ],
        "local_transport":{
            "endpoint_source":"gateway_process_configuration", "authentication":"local_shared_key",
            "cloud_credential_reused":true, "trust_boundary":"vision_and_gateway_share_credentials",
            "negotiation_header":{"X-Estaciona-Local-Delivery":"1"},
            "receive":{"schema":"v1/vision-to-gateway.schema.json"},
            "send":{"schema":"v1/gateway-to-vision.schema.json"},
            "legacy_unwrapped_events_accepted":true, "ack_boundary":"gateway_sqlite_commit"
        }
    })
}
