// ==============================================================================
// Copyright (C) 2026 Guilherme Pedroza
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
// ==============================================================================

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum AppToServerMsg {
    #[serde(rename = "RESERVE_SPOT")]
    ReserveSpot {
        spot_id: String,
        user_id: Uuid,
        plate: String,
    },
    #[serde(rename = "CANCEL_RESERVATION")]
    CancelReservation { reservation_id: Uuid },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum EdgeTransportMsg {
    #[serde(rename = "EDGE_EVENT")]
    Event {
        version: u8,
        event_id: Uuid,
        payload: serde_json::Value,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ServerToEdgeMsg {
    #[serde(rename = "EDGE_READY")]
    Ready { delivery_version: u8 },
    #[serde(rename = "EDGE_ACK")]
    Ack {
        version: u8,
        event_id: Uuid,
        outcome: String,
        reason: Option<String>,
        duplicate: bool,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HourlyOccupancy {
    pub timestamp: DateTime<Utc>,
    pub occupancy: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelHealth {
    pub r2_score: f64,
    pub mae: f64,
    pub rmse: f64,
    pub inference_time_ms: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum EdgeToServerMsg {
    #[serde(rename = "SPOT_UPDATE")]
    SpotUpdate {
        spot_id: String,
        status: String,
        camera_id: String,
        confidence: f64,
        timestamp: DateTime<Utc>,
    },
    #[serde(rename = "CAR_DETECTED")]
    CarDetected {
        plate: String,
        camera_id: String,
        timestamp: DateTime<Utc>,
    },
    #[serde(rename = "PATH_UPDATE")]
    PathUpdate {
        from_node: String,
        to_node: String,
        is_active: bool,
    },
    #[serde(rename = "TREND_PREDICTION")]
    TrendPrediction {
        timestamp: DateTime<Utc>,
        avg_stay_duration_mins: f64,
        stay_duration_distribution: Vec<i32>,
        max_capacity: i32,
        model_health: ModelHealth,
        next_24h_occupancy: Vec<HourlyOccupancy>,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum ServerToAppMsg {
    #[serde(rename = "NAVIGATION_START")]
    NavigationStart { spot_id: String, route: Vec<String> },
    #[serde(rename = "RESERVATION_CONFIRMED")]
    ReservationConfirmed {
        reservation_id: Uuid,
        spot_id: String,
        expires_at: DateTime<Utc>,
    },
    #[serde(rename = "RESERVATION_EXPIRED")]
    ReservationExpired { spot_id: String },
    #[serde(rename = "RESERVATION_REJECTED")]
    ReservationRejected { spot_id: String, reason: String },
    #[serde(rename = "SPOT_UPDATE")]
    SpotUpdate { spot_id: String, status: String },
    #[serde(rename = "TREND_PREDICTION")]
    TrendPrediction {
        timestamp: DateTime<Utc>,
        avg_stay_duration_mins: f64,
        stay_duration_distribution: Vec<i32>,
        max_capacity: i32,
        model_health: ModelHealth,
        next_24h_occupancy: Vec<HourlyOccupancy>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_spot_update_deserializes() {
        let json = r#"{
            "type": "SPOT_UPDATE",
            "spot_id": "A-01",
            "status": "occupied",
            "camera_id": "cam_01",
            "confidence": 0.85,
            "timestamp": "2026-06-04T17:30:00Z"
        }"#;
        let msg: EdgeToServerMsg = serde_json::from_str(json).unwrap();
        match msg {
            EdgeToServerMsg::SpotUpdate {
                spot_id,
                status,
                camera_id,
                confidence,
                ..
            } => {
                assert_eq!(spot_id, "A-01");
                assert_eq!(status, "occupied");
                assert_eq!(camera_id, "cam_01");
                assert!((confidence - 0.85).abs() < f64::EPSILON);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn edge_spot_update_serializes_with_type_tag() {
        let msg = EdgeToServerMsg::SpotUpdate {
            spot_id: "B-05".to_string(),
            status: "free".to_string(),
            camera_id: "cam_02".to_string(),
            confidence: 1.0,
            timestamp: "2026-06-04T18:00:00Z".parse().unwrap(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"SPOT_UPDATE""#));
        assert!(json.contains(r#""spot_id":"B-05""#));
    }

    #[test]
    fn edge_car_detected_roundtrip() {
        let msg = EdgeToServerMsg::CarDetected {
            plate: "ABC1D23".to_string(),
            camera_id: "cam_01".to_string(),
            timestamp: "2026-06-04T18:00:00Z".parse().unwrap(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: EdgeToServerMsg = serde_json::from_str(&json).unwrap();
        match deserialized {
            EdgeToServerMsg::CarDetected {
                plate, camera_id, ..
            } => {
                assert_eq!(plate, "ABC1D23");
                assert_eq!(camera_id, "cam_01");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn edge_path_update_roundtrip() {
        let msg = EdgeToServerMsg::PathUpdate {
            from_node: "entrada".to_string(),
            to_node: "meio-1".to_string(),
            is_active: false,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"PATH_UPDATE""#));
        let deserialized: EdgeToServerMsg = serde_json::from_str(&json).unwrap();
        match deserialized {
            EdgeToServerMsg::PathUpdate {
                from_node,
                to_node,
                is_active,
            } => {
                assert_eq!(from_node, "entrada");
                assert_eq!(to_node, "meio-1");
                assert!(!is_active);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn app_reserve_spot_deserializes() {
        let json = r#"{
            "type": "RESERVE_SPOT",
            "spot_id": "C-10",
            "user_id": "550e8400-e29b-41d4-a716-446655440000",
            "plate": "XYZ9876"
        }"#;
        let msg: AppToServerMsg = serde_json::from_str(json).unwrap();
        match msg {
            AppToServerMsg::ReserveSpot { spot_id, plate, .. } => {
                assert_eq!(spot_id, "C-10");
                assert_eq!(plate, "XYZ9876");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn app_cancel_reservation_roundtrip() {
        let id = Uuid::new_v4();
        let msg = AppToServerMsg::CancelReservation { reservation_id: id };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"CANCEL_RESERVATION""#));
        let deserialized: AppToServerMsg = serde_json::from_str(&json).unwrap();
        match deserialized {
            AppToServerMsg::CancelReservation { reservation_id } => {
                assert_eq!(reservation_id, id);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn server_spot_update_serializes() {
        let msg = ServerToAppMsg::SpotUpdate {
            spot_id: "A-01".to_string(),
            status: "reserved".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"SPOT_UPDATE""#));
        assert!(json.contains(r#""status":"reserved""#));
    }

    #[test]
    fn server_navigation_start_serializes() {
        let msg = ServerToAppMsg::NavigationStart {
            spot_id: "B-03".to_string(),
            route: vec![
                "entrada".to_string(),
                "meio-1".to_string(),
                "B-03".to_string(),
            ],
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"NAVIGATION_START""#));
        assert!(json.contains(r#""route""#));
    }

    #[test]
    fn server_reservation_confirmed_roundtrip() {
        let id = Uuid::new_v4();
        let expires: DateTime<Utc> = "2026-06-04T20:00:00Z".parse().unwrap();
        let msg = ServerToAppMsg::ReservationConfirmed {
            reservation_id: id,
            spot_id: "D-01".to_string(),
            expires_at: expires,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: ServerToAppMsg = serde_json::from_str(&json).unwrap();
        match deserialized {
            ServerToAppMsg::ReservationConfirmed {
                reservation_id,
                spot_id,
                expires_at,
            } => {
                assert_eq!(reservation_id, id);
                assert_eq!(spot_id, "D-01");
                assert_eq!(expires_at, expires);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn server_reservation_expired_serializes() {
        let msg = ServerToAppMsg::ReservationExpired {
            spot_id: "E-05".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"RESERVATION_EXPIRED""#));
    }

    #[test]
    fn server_reservation_rejected_serializes() {
        let msg = ServerToAppMsg::ReservationRejected {
            spot_id: "F-01".to_string(),
            reason: "Spot already taken".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""type":"RESERVATION_REJECTED""#));
        assert!(json.contains("already taken"));
    }

    #[test]
    fn edge_unknown_type_fails() {
        let json = r#"{"type": "UNKNOWN_TYPE", "data": 123}"#;
        let result = serde_json::from_str::<EdgeToServerMsg>(json);
        assert!(result.is_err());
    }

    #[test]
    fn edge_missing_required_field_fails() {
        let json = r#"{"type": "SPOT_UPDATE", "spot_id": "A-01"}"#;
        let result = serde_json::from_str::<EdgeToServerMsg>(json);
        assert!(result.is_err());
    }

    #[test]
    fn edge_trend_prediction_deserializes() {
        let json = r#"{
            "type": "TREND_PREDICTION",
            "timestamp": "2026-06-10T18:00:00Z",
            "avg_stay_duration_mins": 135.5,
            "stay_duration_distribution": [145, 230, 85, 32],
            "max_capacity": 40,
            "model_health": {
                "r2_score": 0.942,
                "mae": 1.8,
                "rmse": 2.4,
                "inference_time_ms": 4.2
            },
            "next_24h_occupancy": []
        }"#;
        let msg: EdgeToServerMsg = serde_json::from_str(json).unwrap();
        match msg {
            EdgeToServerMsg::TrendPrediction {
                stay_duration_distribution,
                max_capacity,
                model_health,
                ..
            } => {
                assert_eq!(stay_duration_distribution, vec![145, 230, 85, 32]);
                assert_eq!(max_capacity, 40);
                assert_eq!(model_health.r2_score, 0.942);
            }
            _ => panic!("wrong variant"),
        }
    }
}
