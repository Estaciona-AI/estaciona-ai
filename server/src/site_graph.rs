// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;

use crate::{
    pathfinding::{DuplicateNodeName, NodeId, ParkingGraph},
    site_config::{ParkingLotConfig, SiteConfig, Spot3DDef},
};

/// Navigation waypoints, one every 0.15 map units: 7.5 km of path. Spots are
/// matched to their nearest waypoint, so both bounds also cap startup work.
pub const MAX_PATH_NODES: usize = 50_000;
pub const MAX_SPOTS: usize = 10_000;

#[derive(Debug)]
pub enum MapGraphError {
    Duplicate(DuplicateNodeName),
    InvalidLayout,
    TooLarge,
}

impl std::fmt::Display for MapGraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Duplicate(error) => error.fmt(f),
            Self::InvalidLayout => f.write_str("Invalid navigation path extent"),
            Self::TooLarge => write!(
                f,
                "Map exceeds {MAX_PATH_NODES} navigation waypoints or {MAX_SPOTS} spots"
            ),
        }
    }
}

impl std::error::Error for MapGraphError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Duplicate(error) => Some(error),
            Self::InvalidLayout | Self::TooLarge => None,
        }
    }
}

impl From<DuplicateNodeName> for MapGraphError {
    fn from(error: DuplicateNodeName) -> Self {
        Self::Duplicate(error)
    }
}

struct Segment {
    origin: (f64, f64),
    delta: (f64, f64),
    steps: usize,
}

// Keep the generated-name range and graph interpolation on the same layout.
// Validation costs O(path points + spots), without expanding all waypoints.
struct Layout {
    origin: Option<(f64, f64)>,
    segments: Vec<Segment>,
    node_count: usize,
}

impl Layout {
    fn new(parkings: &[ParkingLotConfig]) -> Result<Self, MapGraphError> {
        let path = parkings
            .first()
            .map(|lot| lot.path.as_slice())
            .unwrap_or(&[]);
        if path.len() > MAX_PATH_NODES {
            return Err(MapGraphError::TooLarge);
        }
        let origin = path.first().map(|point| (point.x, point.z));
        let mut previous = origin;
        let mut node_count = usize::from(origin.is_some());
        let mut segments = Vec::new();
        for point in path.iter().skip(1) {
            let start = previous.expect("a segment has its initial point");
            let delta = (point.x - start.0, point.z - start.1);
            let count = ((delta.0 * delta.0 + delta.1 * delta.1).sqrt() / 0.15).ceil();
            if !count.is_finite() || count >= usize::MAX as f64 {
                return Err(MapGraphError::InvalidLayout);
            }
            let steps = count as usize;
            node_count = node_count
                .checked_add(steps)
                .ok_or(MapGraphError::InvalidLayout)?;
            // Checked per segment, before any waypoint exists.
            if node_count > MAX_PATH_NODES {
                return Err(MapGraphError::TooLarge);
            }
            segments.push(Segment {
                origin: start,
                delta,
                steps,
            });
            if steps != 0 {
                previous = Some((start.0 + delta.0, start.1 + delta.1));
            }
        }
        Ok(Self {
            origin,
            segments,
            node_count,
        })
    }

    fn validate_names(&self, spots: &[Spot3DDef]) -> Result<(), MapGraphError> {
        if spots.len() > MAX_SPOTS {
            return Err(MapGraphError::TooLarge);
        }
        let mut names = HashSet::new();
        for spot in spots {
            let collides_with_origin = self.origin.is_some() && spot.id == "cam-01";
            let collides_with_waypoint = spot.id.strip_prefix("path_node_").is_some_and(|suffix| {
                suffix.parse::<usize>().is_ok_and(|index| {
                    index > 0 && index < self.node_count && index.to_string() == suffix
                })
            });
            if !names.insert(&spot.id) || collides_with_origin || collides_with_waypoint {
                return Err(DuplicateNodeName {
                    name: spot.id.clone(),
                }
                .into());
            }
        }
        Ok(())
    }
}

pub fn validate_names(
    parkings: &[ParkingLotConfig],
    spots: &[Spot3DDef],
) -> Result<(), MapGraphError> {
    Layout::new(parkings)?.validate_names(spots)
}

pub fn build(site: &SiteConfig) -> Result<ParkingGraph, MapGraphError> {
    let layout = Layout::new(&site.parkings)?;
    layout.validate_names(&site.spots)?;
    let mut graph = ParkingGraph::new();
    let mut path_nodes = Vec::new();
    if let Some((x, z)) = layout.origin {
        add_path_node(&mut graph, &mut path_nodes, x, z)?;
    }
    for segment in layout.segments {
        for step in 1..=segment.steps {
            let fraction = step as f64 / segment.steps as f64;
            add_path_node(
                &mut graph,
                &mut path_nodes,
                segment.origin.0 + segment.delta.0 * fraction,
                segment.origin.1 + segment.delta.1 * fraction,
            )?;
        }
    }
    for spot in &site.spots {
        let node = graph.add_node(&spot.id, spot.center_3d.x as f32, spot.center_3d.z as f32)?;
        let nearest = path_nodes
            .iter()
            .map(|(id, x, z)| {
                let distance =
                    ((x - spot.center_3d.x).powi(2) + (z - spot.center_3d.z).powi(2)).sqrt();
                (*id, distance)
            })
            .min_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((closest, distance)) = nearest {
            graph.add_edge(closest, node, ((distance * 1000.0) as u32).max(1), true);
        }
    }
    Ok(graph)
}

fn add_path_node(
    graph: &mut ParkingGraph,
    nodes: &mut Vec<(NodeId, f64, f64)>,
    x: f64,
    z: f64,
) -> Result<(), MapGraphError> {
    let name = if nodes.is_empty() {
        "cam-01".to_owned()
    } else {
        format!("path_node_{}", nodes.len())
    };
    let node = graph.add_node(&name, x as f32, z as f32)?;
    if let Some((previous, px, pz)) = nodes.last() {
        let distance = ((x - px).powi(2) + (z - pz).powi(2)).sqrt();
        graph.add_edge(*previous, node, ((distance * 1000.0) as u32).max(1), true);
    }
    nodes.push((node, x, z));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::site_config::{Coords3D, PathNode};

    fn site(id: &str, end: f64) -> SiteConfig {
        SiteConfig {
            directory: std::env::temp_dir(),
            parkings: vec![ParkingLotConfig {
                path: vec![PathNode { x: 0.0, z: 0.0 }, PathNode { x: end, z: 0.0 }],
            }],
            spots: vec![Spot3DDef {
                id: id.into(),
                center_3d: Coords3D {
                    x: 0.0,
                    y: 0.0,
                    z: 1.0,
                },
            }],
        }
    }

    #[test]
    fn actual_generated_names_are_rejected() {
        for id in ["cam-01", "path_node_1", "path_node_7"] {
            let site = site(id, 1.0);
            assert!(matches!(
                validate_names(&site.parkings, &site.spots),
                Err(MapGraphError::Duplicate(_))
            ));
            assert!(matches!(build(&site), Err(MapGraphError::Duplicate(_))));
        }
    }

    #[test]
    fn only_actual_names_collide_not_entire_prefixes() {
        for id in [
            "path_node_0",
            "path_node_01",
            "path_node_8",
            "path_node_999",
            "cam-02",
        ] {
            let site = site(id, 1.0);
            validate_names(&site.parkings, &site.spots).unwrap();
            let graph = build(&site).unwrap();
            assert!(graph.calculate_route("cam-01", id).is_some());
        }
    }

    #[test]
    fn repeated_points_do_not_reserve_nonexistent_waypoints() {
        let site = site("path_node_1", 0.0);
        validate_names(&site.parkings, &site.spots).unwrap();
        assert_eq!(
            build(&site)
                .unwrap()
                .calculate_route("cam-01", "path_node_1"),
            Some(vec!["cam-01".into(), "path_node_1".into()])
        );
    }

    #[test]
    fn identifiers_are_local_to_each_graph() {
        let first = build(&site("shared-spot", 1.0)).unwrap();
        let second = build(&site("shared-spot", 2.0)).unwrap();
        assert_eq!(first.calculate_cost("cam-01", "shared-spot"), Some(1000));
        assert_eq!(second.calculate_cost("cam-01", "shared-spot"), Some(1000));
    }

    #[test]
    fn unrepresentable_layout_is_rejected_without_expansion() {
        let site = site("spot", 1e300);
        assert!(matches!(
            validate_names(&site.parkings, &site.spots),
            Err(MapGraphError::InvalidLayout)
        ));
        assert!(matches!(build(&site), Err(MapGraphError::InvalidLayout)));
    }

    fn spots(count: usize) -> Vec<Spot3DDef> {
        (0..count)
            .map(|index| Spot3DDef {
                id: format!("spot-{index}"),
                center_3d: Coords3D {
                    x: 0.0,
                    y: 0.0,
                    z: 1.0,
                },
            })
            .collect()
    }

    #[test]
    fn waypoint_budget_is_exact_and_checked_before_expansion() {
        let step = 0.15;
        let last = (MAX_PATH_NODES - 1) as f64;
        // The origin plus MAX_PATH_NODES - 1 interpolated waypoints fit.
        let at_limit = site("spot", last * step - 0.01);
        let graph = build(&at_limit).unwrap();
        let farthest = format!("path_node_{}", MAX_PATH_NODES - 1);
        assert!(graph.calculate_route("cam-01", &farthest).is_some());
        // One more waypoint does not.
        let over = site("spot", last * step + 0.01);
        assert!(matches!(
            validate_names(&over.parkings, &over.spots),
            Err(MapGraphError::TooLarge)
        ));
        assert!(matches!(build(&over), Err(MapGraphError::TooLarge)));
        // A short map with a wrong scale would expand to billions of nodes.
        let wrong_scale = site("spot", 1e9);
        assert!(matches!(build(&wrong_scale), Err(MapGraphError::TooLarge)));
    }

    #[test]
    fn repeated_path_points_count_against_the_budget() {
        let mut site = site("spot", 0.0);
        site.parkings[0].path = (0..=MAX_PATH_NODES)
            .map(|_| PathNode { x: 0.0, z: 0.0 })
            .collect();
        assert!(matches!(
            validate_names(&site.parkings, &site.spots),
            Err(MapGraphError::TooLarge)
        ));
    }

    #[test]
    fn spot_budget_is_exact() {
        let mut site = site("spot", 1.0);
        site.spots = spots(MAX_SPOTS);
        assert_eq!(
            build(&site).unwrap().calculate_cost("cam-01", "spot-0"),
            Some(1000)
        );
        site.spots = spots(MAX_SPOTS + 1);
        assert!(matches!(
            validate_names(&site.parkings, &site.spots),
            Err(MapGraphError::TooLarge)
        ));
    }
}
