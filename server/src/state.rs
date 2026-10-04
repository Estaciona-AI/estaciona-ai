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

use dashmap::DashMap;
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::{RwLock, broadcast, mpsc};
use uuid::Uuid;

use crate::pathfinding::ParkingGraph;
use crate::site_config::SiteConfig;

pub struct AppState {
    pub pool: PgPool,
    pub tx: broadcast::Sender<String>,
    pub user_sessions: DashMap<Uuid, mpsc::UnboundedSender<String>>,
    pub graph: RwLock<ParkingGraph>,
    pub jwt_secret: String,
    pub plate_pepper: String,
    pub edge_api_key: String,
    pub last_trend_prediction: Mutex<Option<String>>,
    pub site_data_dir: std::path::PathBuf,
}

pub type SharedState = Arc<AppState>;

pub async fn init_state(
    pool: PgPool,
    jwt_secret: String,
    plate_pepper: String,
    edge_api_key: String,
    site: SiteConfig,
) -> SharedState {
    let (tx, _) = broadcast::channel(100);

    let mut graph = ParkingGraph::new();

    let parkings = site.parkings;

    let mut node_map = std::collections::HashMap::new();
    let mut config_nodes: Vec<(String, f64, f64)> = Vec::new();

    if let Some(parking) = parkings.first() {
        let mut prev_node_id: Option<String> = None;
        let mut node_counter = 0;

        for p in parking.path.iter() {
            let mut points_to_add = Vec::new();

            if let Some(_prev_id) = &prev_node_id {
                let prev_p = config_nodes.last().unwrap();
                let dx = p.x - prev_p.1;
                let dz = p.z - prev_p.2;
                let dist = (dx * dx + dz * dz).sqrt();
                let steps = (dist / 0.15).ceil() as usize;

                for step in 1..=steps {
                    let f = step as f64 / steps as f64;
                    points_to_add.push((prev_p.1 + dx * f, prev_p.2 + dz * f));
                }
            } else {
                points_to_add.push((p.x, p.z));
            }

            for (px, pz) in points_to_add {
                let node_id = if node_counter == 0 {
                    "cam-01".to_string()
                } else {
                    format!("path_node_{}", node_counter)
                };

                let graph_node = graph.add_node(&node_id, px as f32, pz as f32);
                node_map.insert(node_id.clone(), graph_node);

                if let Some(prev_id) = &prev_node_id {
                    let from_node = *node_map.get(prev_id).unwrap();
                    let dist_mm = (((px - config_nodes.last().unwrap().1).powi(2)
                        + (pz - config_nodes.last().unwrap().2).powi(2))
                    .sqrt()
                        * 1000.0) as u32;
                    graph.add_edge(from_node, graph_node, dist_mm.max(1), true);
                }

                config_nodes.push((node_id.clone(), px, pz));
                prev_node_id = Some(node_id);
                node_counter += 1;
            }
        }
    }

    let spots_3d = site.spots;

    for spot in spots_3d {
        sqlx::query!(
            r#"
            INSERT INTO spots (id, parking_lot, status, x, y, z, last_updated) 
            VALUES ($1, 'Main', 'free', $2, $3, $4, NOW()) 
            ON CONFLICT (id) DO NOTHING
            "#,
            spot.id,
            spot.center_3d.x,
            spot.center_3d.y,
            spot.center_3d.z
        )
        .execute(&pool)
        .await
        .expect("Falha ao sincronizar vaga com o banco de dados");

        let spot_node = graph.add_node(&spot.id, spot.center_3d.x as f32, spot.center_3d.z as f32);

        if let Some(parking) = parkings.first()
            && !parking.path.is_empty()
        {
            let nearest = config_nodes
                .iter()
                .filter_map(|(name, x, z)| {
                    let distance =
                        ((x - spot.center_3d.x).powi(2) + (z - spot.center_3d.z).powi(2)).sqrt();
                    node_map.get(name).map(|node| (*node, distance))
                })
                .min_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((closest, distance)) = nearest {
                graph.add_edge(
                    closest,
                    spot_node,
                    ((distance * 1000.0) as u32).max(1),
                    true,
                );
            }
        }
    }

    Arc::new(AppState {
        pool,
        tx,
        user_sessions: DashMap::new(),
        graph: RwLock::new(graph),
        jwt_secret,
        plate_pepper,
        edge_api_key,
        last_trend_prediction: Mutex::new(None),
        site_data_dir: site.directory,
    })
}
