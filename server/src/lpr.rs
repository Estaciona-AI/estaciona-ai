// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use sqlx::Acquire;

use crate::{
    edge_auth::{self, AuthError, CameraOperation, Principal},
    reservation_store::{self, CreateError},
    state::SharedState,
    ws::messages::ServerToAppMsg,
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LprMode {
    #[default]
    Disabled,
    Enabled,
}

impl LprMode {
    pub fn parse(value: Option<&str>) -> Result<Self, &'static str> {
        match value {
            None | Some("false") => Ok(Self::Disabled),
            Some("true") => Ok(Self::Enabled),
            _ => Err("LPR_ENABLED must be true or false"),
        }
    }
}

pub async fn car_detected(
    state: &SharedState,
    principal: &Principal,
    plate: &str,
    camera_id: &str,
) -> Result<bool, AuthError> {
    if state.installation.lpr_mode == LprMode::Disabled {
        return Ok(true);
    }
    let mut tx = edge_auth::begin_authorized(state.installation.pool(), principal).await?;
    if !edge_auth::camera_allowed(&mut tx, principal, camera_id, CameraOperation::CarDetected)
        .await?
    {
        tx.rollback().await?;
        return Ok(false);
    }
    let hashed_plate = crate::security::hash_plate(plate, &state.plate_pepper);
    let user = crate::vehicles::approved_user(&mut tx, &hashed_plate).await?;
    let Some(user_id) = user else {
        tx.rollback().await?;
        return Ok(true);
    };
    let reserved = sqlx::query_scalar::<_, String>(
        "SELECT spot_id FROM reservations WHERE user_id = $1 AND status = 'active' LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (spot_id, route, newly_reserved) = if let Some(spot) = reserved {
        let granted: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM edge_camera_spots AS grants JOIN spots ON spots.id=grants.spot_id WHERE gateway_id = $1 AND spot_id = $2 AND spots.is_active)",
        )
        .bind(principal.gateway_id)
        .bind(&spot)
        .fetch_one(&mut *tx)
        .await?;
        if !granted {
            tx.rollback().await?;
            return Ok(true);
        }
        let route = state
            .installation
            .graph
            .read()
            .await
            .calculate_route(camera_id, &spot);
        (spot, route, false)
    } else {
        let free: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM spots WHERE status = 'free' AND is_active AND EXISTS( \
             SELECT 1 FROM edge_camera_spots WHERE gateway_id = $1 AND spot_id = spots.id)",
        )
        .bind(principal.gateway_id)
        .fetch_all(&mut *tx)
        .await?;
        let candidates = {
            let graph = state.installation.graph.read().await;
            let mut candidates: Vec<_> = free
                .into_iter()
                .filter_map(|spot| {
                    Some((
                        graph.calculate_cost(camera_id, &spot)?,
                        spot.clone(),
                        graph.calculate_route(camera_id, &spot)?,
                    ))
                })
                .collect();
            candidates.sort_by_key(|(cost, _, _)| *cost);
            candidates
        };
        let mut claimed = None;
        for (_, spot, route) in candidates {
            // A failed candidate may abort its SQL transaction. A savepoint keeps
            // the authority lease intact while the next candidate is considered.
            let mut attempt = tx.begin().await?;
            match reservation_store::create_in_transaction(
                &mut attempt,
                &spot,
                user_id,
                &state.installation.reservation_policy,
            )
            .await
            {
                Ok(_) => {
                    attempt.commit().await?;
                    claimed = Some((spot, Some(route), true));
                    break;
                }
                Err(CreateError::UserLimit) => {
                    attempt.rollback().await?;
                    tx.rollback().await?;
                    return Ok(true);
                }
                Err(CreateError::Unavailable) => attempt.rollback().await?,
                Err(CreateError::Database(error)) => return Err(error.into()),
            }
        }
        let Some(claimed) = claimed else {
            tx.rollback().await?;
            return Ok(true);
        };
        claimed
    };
    // Revocation/policy replacement waits for this authority lease until commit.
    // Only committed actions may publish occupancy or private navigation.
    tx.commit().await?;
    if newly_reserved {
        let update = ServerToAppMsg::SpotUpdate {
            spot_id: spot_id.clone(),
            status: "reserved".into(),
        };
        if let Ok(json) = serde_json::to_string(&update) {
            let _ = state.installation.tx.send(json);
        }
    }
    if let Some(session) = state.installation.user_sessions.get(&user_id) {
        let navigation = match route {
            Some(route) => ServerToAppMsg::NavigationStart { spot_id, route },
            None => ServerToAppMsg::NavigationUnavailable {
                spot_id,
                reason: "route_unavailable".into(),
            },
        };
        if let Ok(json) = serde_json::to_string(&navigation) {
            let _ = session.try_send(json);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod mode_tests {
    use super::LprMode;
    #[test]
    fn lpr_is_disabled_unless_explicitly_enabled() {
        assert_eq!(LprMode::parse(None).unwrap(), LprMode::Disabled);
        assert_eq!(LprMode::parse(Some("false")).unwrap(), LprMode::Disabled);
        assert_eq!(LprMode::parse(Some("true")).unwrap(), LprMode::Enabled);
        for invalid in ["", "1", "TRUE", "enabled"] {
            assert!(LprMode::parse(Some(invalid)).is_err());
        }
    }
}
