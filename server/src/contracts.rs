//! Versioned wire artifacts generated from the same DTOs used by the server.
use std::{collections::BTreeMap, io, path::Path};

use schemars::{
    JsonSchema, Schema, SchemaGenerator,
    generate::{Contract, SchemaSettings},
    json_schema,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ws::messages::{
    AppToServerMsg, EdgeToServerMsg, EdgeTransportMsg, ServerToAppMsg, ServerToEdgeMsg,
};

pub const VERSION: &str = "1.0.0";

pub fn physical_state_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"string","enum":["free","occupied"]})
}

pub fn spot_status_schema(_: &mut SchemaGenerator) -> Schema {
    json_schema!({"type":"string","enum":["free","occupied","reserved"]})
}

// Mirrors the local Python transport. Interoperability tests exercise the actual
// outbox and gateway encoder; these types are artifact DTOs, not a Rust receiver.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type")]
pub enum VisionToGateway {
    #[serde(rename = "LOCAL_EVENT")]
    Event {
        #[schemars(range(min = 1, max = 1))]
        version: u8,
        event_id: Uuid,
        payload: EdgeToServerMsg,
    },
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LocalOutcome {
    Persisted,
    Duplicate,
    Rejected,
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "type")]
pub enum GatewayToVision {
    #[serde(rename = "LOCAL_READY")]
    Ready {
        #[schemars(range(min = 1, max = 1))]
        delivery_version: u8,
    },
    #[serde(rename = "LOCAL_ACK")]
    Ack {
        #[schemars(range(min = 1, max = 1))]
        version: u8,
        event_id: Uuid,
        outcome: LocalOutcome,
    },
}

fn id(channel: &str) -> String {
    format!("urn:estaciona-ai:ws:{channel}:v1")
}

fn schema<T: JsonSchema>(channel: &str, contract: Contract) -> Value {
    let mut settings = SchemaSettings::draft2020_12();
    settings.contract = contract;
    let mut value = serde_json::to_value(settings.into_generator().into_root_schema_for::<T>())
        .expect("schema serializes");
    value["$id"] = json!(id(channel));
    value["x-contract-version"] = json!(VERSION);
    value
}

pub fn schemas() -> BTreeMap<&'static str, Value> {
    BTreeMap::from([
        (
            "app-to-server",
            schema::<AppToServerMsg>("app-to-server", Contract::Deserialize),
        ),
        (
            "server-to-app",
            schema::<ServerToAppMsg>("server-to-app", Contract::Serialize),
        ),
        (
            "edge-events",
            schema::<EdgeToServerMsg>("edge-events", Contract::Deserialize),
        ),
        (
            "gateway-to-server",
            schema::<EdgeTransportMsg>("gateway-to-server", Contract::Deserialize),
        ),
        (
            "server-to-gateway",
            schema::<ServerToEdgeMsg>("server-to-gateway", Contract::Serialize),
        ),
        (
            "vision-to-gateway",
            schema::<VisionToGateway>("vision-to-gateway", Contract::Deserialize),
        ),
        (
            "gateway-to-vision",
            schema::<GatewayToVision>("gateway-to-vision", Contract::Serialize),
        ),
    ])
}

fn root_schema(schemas: &BTreeMap<&str, Value>) -> Value {
    let mut root = json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "$id":"urn:estaciona-ai:ws:messages:v1",
        "title":"Estaciona AI websocket contracts",
        "x-contract-version":VERSION,
        // Directions intentionally overlap: a public SPOT_UPDATE is also part
        // of an edge observation. oneOf across directions would reject it.
        "anyOf":schemas.keys().map(|name|json!({"$ref":id(name)})).collect::<Vec<_>>(),
        "$defs":{}
    });
    // Preserve historical JSON pointers while referring to generated variants.
    for (channel, prefix) in [
        ("edge-events", "EdgeToServer"),
        ("app-to-server", "AppToServer"),
        ("server-to-app", "ServerToApp"),
    ] {
        let schema = &schemas[channel];
        let union = if schema.get("oneOf").is_some() {
            "oneOf"
        } else {
            "anyOf"
        };
        if let Some(variants) = schema[union].as_array() {
            for (index, variant) in variants.iter().enumerate() {
                if let Some(tag) = variant
                    .pointer("/properties/type/const")
                    .and_then(Value::as_str)
                {
                    root["$defs"][format!("{prefix}_{tag}")] =
                        json!({"$ref":format!("{}#/{union}/{index}",id(channel))});
                }
            }
        }
    }
    root
}

fn fixture<T: Serialize>(value: T) -> Value {
    serde_json::to_value(value).expect("fixture serializes")
}

pub fn fixtures() -> Value {
    let timestamp = "2026-10-01T12:00:00Z".parse().expect("fixed timestamp");
    let event_id = Uuid::parse_str("00000000-0000-4000-8000-000000000001").expect("fixed UUID");
    let edge = EdgeToServerMsg::SpotUpdate {
        spot_id: "A-01".into(),
        status: "occupied".into(),
        camera_id: "camera-01".into(),
        confidence: 0.9,
        timestamp,
    };
    let forecast = EdgeToServerMsg::TrendPrediction {
        timestamp,
        avg_stay_duration_mins: 60.0,
        stay_duration_distribution: vec![1, 2, 3],
        max_capacity: 400,
        model_health: crate::ws::messages::ModelHealth {
            r2_score: 0.8,
            mae: 1.0,
            rmse: 2.0,
            inference_time_ms: 3.0,
        },
        next_24h_occupancy: vec![crate::ws::messages::HourlyOccupancy {
            timestamp,
            occupancy: 200,
        }],
    };
    // Public and edge forecast DTOs intentionally share the existing shape.
    let public: ServerToAppMsg =
        serde_json::from_value(fixture(forecast.clone())).expect("shared forecast shape");
    json!({
        "app-to-server":[
            fixture(AppToServerMsg::ReserveSpot{spot_id:"A-01".into(),user_id:None,plate:None}),
            fixture(AppToServerMsg::ReserveSpot{spot_id:"A-01".into(),user_id:Some(event_id),plate:Some("EXAMPLE".into())}),
            fixture(AppToServerMsg::CancelReservation{reservation_id:event_id})
        ],
        "edge-events":[fixture(edge.clone()),fixture(EdgeToServerMsg::CarDetected{plate:"EXAMPLE".into(),camera_id:"camera-01".into(),timestamp}),fixture(EdgeToServerMsg::PathUpdate{from_node:"camera-01".into(),to_node:"A-01".into(),is_active:true}),fixture(forecast)],
        "gateway-to-server":[fixture(EdgeTransportMsg::Event{version:1,event_id,payload:fixture(edge.clone())})],
        "server-to-gateway":[fixture(ServerToEdgeMsg::Ready{delivery_version:1,durable_events:vec!["SPOT_UPDATE","PATH_UPDATE","TREND_PREDICTION","CAR_DETECTED"]}),fixture(ServerToEdgeMsg::Ack{version:1,event_id,outcome:"applied".into(),reason:None,duplicate:false})],
        "vision-to-gateway":[fixture(VisionToGateway::Event{version:1,event_id,payload:edge})],
        "gateway-to-vision":[fixture(GatewayToVision::Ready{delivery_version:1}),fixture(GatewayToVision::Ack{version:1,event_id,outcome:LocalOutcome::Persisted}),fixture(GatewayToVision::Ack{version:1,event_id,outcome:LocalOutcome::Duplicate}),fixture(GatewayToVision::Ack{version:1,event_id,outcome:LocalOutcome::Rejected})],
        "server-to-app":[fixture(ServerToAppMsg::SpotUpdate{spot_id:"A-01".into(),status:"reserved".into()}),fixture(ServerToAppMsg::NavigationStart{spot_id:"A-01".into(),route:vec!["camera-01".into(),"A-01".into()]}),fixture(ServerToAppMsg::NavigationUnavailable{spot_id:"A-01".into(),reason:"no_route".into()}),fixture(ServerToAppMsg::ReservationConfirmed{reservation_id:event_id,spot_id:"A-01".into(),expires_at:timestamp}),fixture(ServerToAppMsg::ReservationExpired{spot_id:"A-01".into()}),fixture(ServerToAppMsg::ReservationRejected{spot_id:"A-01".into(),reason:"active_reservation_limit".into()}),fixture(public)]
    })
}

pub fn artifacts() -> BTreeMap<String, Value> {
    let schemas = schemas();
    let mut artifacts = BTreeMap::new();
    artifacts.insert("messages.json".into(), root_schema(&schemas));
    artifacts.insert("v1/manifest.json".into(),json!({"version":VERSION,"wire_version":1,"channels":schemas.keys().map(|name|(name.to_string(),id(name))).collect::<BTreeMap<_,_>>(),"compatibility":{"identity_source":"authenticated_session","tenant_scope":"single_installation","local_ack":"gateway_sqlite_commit","cloud_ack":"postgres_commit","freshness":"strictly_newer_source_timestamp","maximum_future_clock_skew_seconds":30}}));
    artifacts.insert("v1/fixtures.json".into(), fixtures());
    for (name, schema) in schemas {
        artifacts.insert(format!("v1/{name}.schema.json"), schema);
    }
    artifacts
}

pub fn export(directory: &Path) -> io::Result<()> {
    for (name, value) in artifacts() {
        let path = directory.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut encoded = serde_json::to_string_pretty(&value).map_err(io::Error::other)?;
        encoded.push('\n');
        std::fs::write(path, encoded)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_contracts_match_checked_in_artifacts() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../shared");
        for (name, expected) in artifacts() {
            let path = root.join(name);
            let actual: Value = serde_json::from_slice(
                &std::fs::read(&path).expect("regenerate contracts with --export-contracts"),
            )
            .expect("valid checked-in JSON");
            assert_eq!(
                actual,
                expected,
                "{} differs from Rust DTOs",
                path.display()
            );
        }
    }
}
