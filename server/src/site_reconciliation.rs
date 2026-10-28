// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use std::{collections::HashSet, fmt::Write};

use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::site_config::SiteConfig;

#[derive(Debug)]
pub enum ReconcileError {
    InstallationMismatch,
    Database(sqlx::Error),
    InvalidMap(String),
    ActiveReservations(Vec<String>),
}

impl std::fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InstallationMismatch => {
                f.write_str("Deployment package does not match the verified installation")
            }
            Self::Database(_) => f.write_str("Map reconciliation database failure"),
            Self::InvalidMap(reason) => write!(f, "Invalid installation map: {reason}"),
            Self::ActiveReservations(spots) => write!(
                f,
                "Cannot retire spots with active reservations: {}",
                spots.join(", ")
            ),
        }
    }
}
impl std::error::Error for ReconcileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
            _ => None,
        }
    }
}
impl From<sqlx::Error> for ReconcileError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

pub async fn apply(pool: &PgPool, site: &SiteConfig) -> Result<i64, ReconcileError> {
    let mut unique = HashSet::new();
    let mut spots: Vec<_> = site.spots.iter().collect();
    spots.sort_by(|a, b| a.id.cmp(&b.id));
    for spot in &spots {
        if spot.id.trim().is_empty()
            || spot.id.contains('\0')
            || !unique.insert(&spot.id)
            || ![spot.center_3d.x, spot.center_3d.y, spot.center_3d.z]
                .iter()
                .all(|v| v.is_finite())
        {
            return Err(ReconcileError::InvalidMap(
                "Duplicate/empty ID or invalid coordinates".into(),
            ));
        }
    }
    // Sector of each spot: the lot it names, or the first lot.
    let sector = |spot: &crate::site_config::Spot3DDef| -> String {
        spot.parking.clone().unwrap_or_else(|| {
            site.parkings
                .first()
                .map_or_else(|| "Main".to_owned(), |lot| lot.name.clone())
        })
    };
    let manifest = serde_json::json!({
        "parkings": site.parkings.iter().map(|lot| serde_json::json!({
            "name": lot.name,
            "path": lot.path.iter().map(|p| serde_json::json!({"x":p.x,"z":p.z})).collect::<Vec<_>>()
        })).collect::<Vec<_>>(),
        "spots": spots.iter().map(|s| {
            let mut spot = serde_json::json!({"id":s.id,
                "center3D":{"x":s.center_3d.x,"y":s.center_3d.y,"z":s.center_3d.z},
                "parking": sector(s)});
            if let Some(kept_for) = s.reserved_for {
                spot["reserved_for"] = kept_for.as_str().into();
            }
            spot
        }).collect::<Vec<_>>()
    });
    let mut fingerprint = String::with_capacity(64);
    for byte in Sha256::digest(manifest.to_string().as_bytes()) {
        write!(&mut fingerprint, "{byte:02x}").expect("writing to a String cannot fail");
    }
    let ids: Vec<&str> = spots.iter().map(|s| s.id.as_str()).collect();
    let mut tx = pool.begin().await?;
    sqlx::raw_sql("SET LOCAL lock_timeout = '5s'; SET LOCAL statement_timeout = '10s'")
        .execute(&mut *tx)
        .await?;
    // Exclusive conflicts with writers and SELECT FOR UPDATE/KEY SHARE. Taking
    // it before inspecting reservations preserves the existing spot-first order.
    sqlx::query("LOCK TABLE spots IN EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await?;
    let blocked: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT spot_id FROM reservations WHERE status = 'active' \
         AND NOT (spot_id = ANY($1)) ORDER BY spot_id",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?;
    if !blocked.is_empty() {
        return Err(ReconcileError::ActiveReservations(blocked));
    }
    for spot in spots {
        sqlx::query(
            "INSERT INTO spots (id,parking_lot,status,x,y,z,is_active,reserved_for) VALUES ($1,$5,'free',$2,$3,$4,TRUE,$6) \
             ON CONFLICT (id) DO UPDATE SET x=EXCLUDED.x,y=EXCLUDED.y,z=EXCLUDED.z,is_active=TRUE, \
             parking_lot=EXCLUDED.parking_lot,reserved_for=EXCLUDED.reserved_for \
             WHERE (spots.x,spots.y,spots.z,spots.is_active,spots.parking_lot,spots.reserved_for) \
             IS DISTINCT FROM (EXCLUDED.x,EXCLUDED.y,EXCLUDED.z,TRUE,EXCLUDED.parking_lot,EXCLUDED.reserved_for)"
        ).bind(&spot.id).bind(spot.center_3d.x).bind(spot.center_3d.y).bind(spot.center_3d.z)
            .bind(sector(spot)).bind(spot.reserved_for.map(|kept_for| kept_for.as_str()))
            .execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE spots SET is_active=FALSE WHERE is_active AND NOT (id=ANY($1))")
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    let previous: Option<(i64, String)> = sqlx::query_as(
        "SELECT revision,fingerprint FROM site_map_revisions ORDER BY revision DESC LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await?;
    let revision = match previous {
        Some((revision, old)) if old == fingerprint => revision,
        _ => sqlx::query_scalar("INSERT INTO site_map_revisions (fingerprint,manifest) VALUES ($1,$2) RETURNING revision")
            .bind(fingerprint).bind(manifest).fetch_one(&mut *tx).await?,
    };
    tx.commit().await?;
    Ok(revision)
}
