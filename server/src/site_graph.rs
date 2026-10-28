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

struct LotLayout {
    origin: Option<(f64, f64)>,
    segments: Vec<Segment>,
}

// Keep the generated-name range and graph interpolation on the same layout.
// Validation costs O(path points + spots), without expanding all waypoints.
struct Layout {
    lots: Vec<LotLayout>,
    node_count: usize,
}

/// The first lot keeps the names edges already report (PATH_UPDATE uses
/// "cam-01" and "path_node_N"); other lots enter at "entrance-N".
fn entrance_name(lot: usize) -> String {
    if lot == 0 {
        "cam-01".to_owned()
    } else {
        format!("entrance-{lot}")
    }
}

impl Layout {
    fn new(parkings: &[ParkingLotConfig]) -> Result<Self, MapGraphError> {
        let points: usize = parkings.iter().map(|lot| lot.path.len()).sum();
        if points > MAX_PATH_NODES {
            return Err(MapGraphError::TooLarge);
        }
        let mut node_count = 0usize;
        let mut lots = Vec::with_capacity(parkings.len());
        for lot in parkings {
            let origin = lot.path.first().map(|point| (point.x, point.z));
            let mut previous = origin;
            node_count += usize::from(origin.is_some());
            let mut segments = Vec::new();
            for point in lot.path.iter().skip(1) {
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
            lots.push(LotLayout { origin, segments });
        }
        Ok(Self { lots, node_count })
    }

    fn validate_names(&self, spots: &[Spot3DDef]) -> Result<(), MapGraphError> {
        if spots.len() > MAX_SPOTS {
            return Err(MapGraphError::TooLarge);
        }
        let entrances: HashSet<String> = (0..self.lots.len())
            .filter(|&lot| self.lots[lot].origin.is_some())
            .map(entrance_name)
            .collect();
        let mut names = HashSet::new();
        for spot in spots {
            let collides_with_waypoint = spot.id.strip_prefix("path_node_").is_some_and(|suffix| {
                suffix.parse::<usize>().is_ok_and(|index| {
                    index > 0 && index < self.node_count && index.to_string() == suffix
                })
            });
            if !names.insert(&spot.id) || entrances.contains(&spot.id) || collides_with_waypoint {
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
    // Waypoints of every lot, numbered across lots; lots are not connected.
    let mut path_nodes: Vec<(NodeId, f64, f64)> = Vec::new();
    for (index, lot) in layout.lots.into_iter().enumerate() {
        let Some((x, z)) = lot.origin else {
            continue;
        };
        let entrance = entrance_name(index);
        let mut previous = (graph.add_node(&entrance, x as f32, z as f32)?, x, z);
        graph.mark_entrance(&entrance);
        path_nodes.push(previous);
        for segment in lot.segments {
            for step in 1..=segment.steps {
                let fraction = step as f64 / segment.steps as f64;
                let x = segment.origin.0 + segment.delta.0 * fraction;
                let z = segment.origin.1 + segment.delta.1 * fraction;
                let name = format!("path_node_{}", path_nodes.len());
                let node = graph.add_node(&name, x as f32, z as f32)?;
                let distance = ((x - previous.1).powi(2) + (z - previous.2).powi(2)).sqrt();
                graph.add_edge(previous.0, node, ((distance * 1000.0) as u32).max(1), true);
                previous = (node, x, z);
                path_nodes.push(previous);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::site_config::{Coords3D, PathNode};

    fn site(id: &str, end: f64) -> SiteConfig {
        SiteConfig {
            directory: std::env::temp_dir(),
            parkings: vec![ParkingLotConfig {
                name: "Lot".into(),
                path: vec![PathNode { x: 0.0, z: 0.0 }, PathNode { x: end, z: 0.0 }],
            }],
            spots: vec![Spot3DDef {
                id: id.into(),
                center_3d: Coords3D {
                    x: 0.0,
                    y: 0.0,
                    z: 1.0,
                },
                reserved_for: None,
                parking: None,
            }],
        }
    }

    #[test]
    fn route_costs_follow_the_map_geometry() {
        // A 0.5 m path that does not start at the origin, so a mirrored or
        // shifted interpolation cannot reproduce the same distances.
        let mut site = site("A-01", 0.0);
        site.parkings[0].path = vec![PathNode { x: 1.0, z: 1.0 }, PathNode { x: 1.3, z: 1.4 }];
        site.spots[0].center_3d = Coords3D {
            x: 1.3,
            y: 0.0,
            z: 1.5,
        };
        let graph = build(&site).unwrap();
        let (cost, route) = graph.route_from_nearest_entrance("A-01").unwrap();
        // Four 0.125 m waypoints, then 0.1 m to the spot, in millimetres.
        assert_eq!(
            route,
            [
                "cam-01",
                "path_node_1",
                "path_node_2",
                "path_node_3",
                "path_node_4",
                "A-01"
            ]
        );
        assert!((595..=600).contains(&cost), "{cost}");
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
                reserved_for: None,
                parking: None,
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

    fn spot(id: &str, x: f64) -> Spot3DDef {
        Spot3DDef {
            id: id.into(),
            center_3d: Coords3D { x, y: 0.0, z: 1.0 },
            reserved_for: None,
            parking: None,
        }
    }

    fn two_lots(spots: Vec<Spot3DDef>) -> SiteConfig {
        let lot = |name: &str, x: f64| ParkingLotConfig {
            name: name.into(),
            path: vec![PathNode { x, z: 0.0 }, PathNode { x: x + 1.0, z: 0.0 }],
        };
        SiteConfig {
            directory: std::env::temp_dir(),
            parkings: vec![lot("North", 0.0), lot("South", 100.0)],
            spots,
        }
    }

    #[test]
    fn every_lot_has_an_entrance_and_spots_join_the_nearest_lot() {
        let graph = build(&two_lots(vec![
            spot("north-1", 0.5),
            spot("south-1", 100.5),
        ]))
        .unwrap();
        let (_, north) = graph.route_from_nearest_entrance("north-1").unwrap();
        let (_, south) = graph.route_from_nearest_entrance("south-1").unwrap();
        assert_eq!(north.first().map(String::as_str), Some("cam-01"));
        assert_eq!(south.first().map(String::as_str), Some("entrance-1"));
        // Lots are not connected: the north entrance does not reach the south.
        assert_eq!(graph.calculate_route("cam-01", "south-1"), None);
    }

    #[test]
    fn a_spot_cannot_take_an_entrance_name() {
        let site = two_lots(vec![spot("entrance-1", 0.5)]);
        assert!(matches!(
            validate_names(&site.parkings, &site.spots),
            Err(MapGraphError::Duplicate(_))
        ));
        let site = two_lots(vec![spot("entrance-2", 0.5)]);
        validate_names(&site.parkings, &site.spots).unwrap();
    }

    #[test]
    fn the_waypoint_budget_counts_every_lot() {
        let mut site = two_lots(vec![spot("a", 0.0)]);
        let half = (MAX_PATH_NODES / 2) as f64 * 0.15;
        for lot in &mut site.parkings {
            lot.path[1].x = lot.path[0].x + half;
        }
        assert!(matches!(build(&site), Err(MapGraphError::TooLarge)));
    }
}
