// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(pub usize);

#[derive(Debug, PartialEq, Eq)]
pub struct DuplicateNodeName {
    pub(crate) name: String,
}

impl std::fmt::Display for DuplicateNodeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Duplicate navigation node name: {}", self.name)
    }
}

impl std::error::Error for DuplicateNodeName {}

#[derive(Debug, Clone)]
pub struct Node {
    pub name: String,
    #[allow(dead_code)] // Coordinates remain available to diagnostics and future graph display.
    pub x: f32,
    #[allow(dead_code)]
    pub y: f32,
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub target: NodeId,
    pub cost: u32,
    pub active: bool,
}

#[derive(Copy, Clone, Eq, PartialEq)]
struct State {
    cost: u32,
    node: NodeId,
}

impl Ord for State {
    fn cmp(&self, other: &Self) -> Ordering {
        other.cost.cmp(&self.cost)
    }
}

impl PartialOrd for State {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

pub struct ParkingGraph {
    nodes: HashMap<NodeId, Node>,
    edges: HashMap<NodeId, Vec<Edge>>,
    name_to_id: HashMap<String, NodeId>,
    next_id: usize,
}

impl ParkingGraph {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            edges: HashMap::new(),
            name_to_id: HashMap::new(),
            next_id: 0,
        }
    }

    pub fn add_node(&mut self, name: &str, x: f32, y: f32) -> Result<NodeId, DuplicateNodeName> {
        if self.name_to_id.contains_key(name) {
            return Err(DuplicateNodeName {
                name: name.to_owned(),
            });
        }
        let id = NodeId(self.next_id);
        self.next_id += 1;

        self.nodes.insert(
            id,
            Node {
                name: name.to_string(),
                x,
                y,
            },
        );
        self.name_to_id.insert(name.to_string(), id);
        self.edges.insert(id, Vec::new());

        Ok(id)
    }

    pub fn add_edge(&mut self, from: NodeId, to: NodeId, cost: u32, bidirectional: bool) {
        if let Some(neighbors) = self.edges.get_mut(&from) {
            neighbors.push(Edge {
                target: to,
                cost,
                active: true,
            });
        }
        if bidirectional && let Some(neighbors) = self.edges.get_mut(&to) {
            neighbors.push(Edge {
                target: from,
                cost,
                active: true,
            });
        }
    }

    /// Identifies topology and geometry, independently of transient edge status.
    pub(crate) fn revision(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut nodes: Vec<_> = self
            .nodes
            .values()
            .map(|node| (&node.name, node.x.to_bits(), node.y.to_bits()))
            .collect();
        nodes.sort();
        let mut edges: Vec<_> = self
            .edges
            .iter()
            .flat_map(|(from, edges)| {
                edges.iter().map(move |edge| {
                    (
                        &self.nodes[from].name,
                        &self.nodes[&edge.target].name,
                        edge.cost,
                    )
                })
            })
            .collect();
        edges.sort();
        let bytes = serde_json::to_vec(&(1u8, nodes, edges)).expect("graph primitives serialize");
        use std::fmt::Write;
        let mut revision = String::with_capacity(64);
        for byte in Sha256::digest(bytes) {
            write!(&mut revision, "{byte:02x}").expect("String writes are infallible");
        }
        revision
    }

    pub(crate) fn has_edge(&self, from: &str, to: &str) -> bool {
        let (Some(from), Some(to)) = (self.name_to_id.get(from), self.name_to_id.get(to)) else {
            return false;
        };
        self.edges
            .get(from)
            .is_some_and(|edges| edges.iter().any(|edge| edge.target == *to))
    }

    pub fn set_edge_status(&mut self, from: &str, to: &str, active: bool) {
        let from_id = self.name_to_id.get(from).copied();
        let to_id = self.name_to_id.get(to).copied();

        if let (Some(f_id), Some(t_id)) = (from_id, to_id) {
            if let Some(neighbors) = self.edges.get_mut(&f_id) {
                for edge in neighbors.iter_mut() {
                    if edge.target == t_id {
                        edge.active = active;
                    }
                }
            }
            if let Some(neighbors) = self.edges.get_mut(&t_id) {
                for edge in neighbors.iter_mut() {
                    if edge.target == f_id {
                        edge.active = active;
                    }
                }
            }
        }
    }

    pub fn calculate_cost(&self, start_name: &str, end_name: &str) -> Option<u32> {
        let start = *self.name_to_id.get(start_name)?;
        let goal = *self.name_to_id.get(end_name)?;

        let mut frontier = BinaryHeap::new();
        frontier.push(State {
            cost: 0,
            node: start,
        });

        let mut cost_so_far: HashMap<NodeId, u32> = HashMap::new();
        cost_so_far.insert(start, 0);

        while let Some(State { node: current, .. }) = frontier.pop() {
            if current == goal {
                return cost_so_far.get(&goal).copied();
            }

            if let Some(neighbors) = self.edges.get(&current) {
                for edge in neighbors {
                    if !edge.active {
                        continue;
                    }

                    let next = edge.target;
                    let new_cost = cost_so_far.get(&current).unwrap_or(&0) + edge.cost;

                    if !cost_so_far.contains_key(&next)
                        || new_cost < *cost_so_far.get(&next).unwrap()
                    {
                        cost_so_far.insert(next, new_cost);
                        let priority = new_cost;

                        frontier.push(State {
                            cost: priority,
                            node: next,
                        });
                    }
                }
            }
        }

        None
    }

    pub fn calculate_route(&self, start_name: &str, end_name: &str) -> Option<Vec<String>> {
        let start = *self.name_to_id.get(start_name)?;
        let goal = *self.name_to_id.get(end_name)?;

        let mut frontier = BinaryHeap::new();
        frontier.push(State {
            cost: 0,
            node: start,
        });

        let mut came_from: HashMap<NodeId, Option<NodeId>> = HashMap::new();
        let mut cost_so_far: HashMap<NodeId, u32> = HashMap::new();

        came_from.insert(start, None);
        cost_so_far.insert(start, 0);

        while let Some(State { node: current, .. }) = frontier.pop() {
            if current == goal {
                break;
            }

            if let Some(neighbors) = self.edges.get(&current) {
                for edge in neighbors {
                    // Impede o algoritmo de atravessar arestas bloqueadas
                    if !edge.active {
                        continue;
                    }

                    let next = edge.target;
                    let new_cost = cost_so_far.get(&current).unwrap_or(&0) + edge.cost;

                    if !cost_so_far.contains_key(&next)
                        || new_cost < *cost_so_far.get(&next).unwrap()
                    {
                        cost_so_far.insert(next, new_cost);
                        let priority = new_cost;

                        frontier.push(State {
                            cost: priority,
                            node: next,
                        });
                        came_from.insert(next, Some(current));
                    }
                }
            }
        }

        let mut current = goal;
        let mut path_ids = vec![current];

        while let Some(Some(prev)) = came_from.get(&current) {
            current = *prev;
            path_ids.push(current);
        }

        if current != start {
            return None;
        }

        path_ids.reverse();

        Some(
            path_ids
                .into_iter()
                .map(|id| self.nodes.get(&id).unwrap().name.clone())
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_test_graph() -> ParkingGraph {
        let mut graph = ParkingGraph::new();

        let in_node = graph.add_node("entrada", 0.0, 0.0).unwrap();
        let mid1_node = graph.add_node("meio-1", 1.0, 0.0).unwrap();
        let mid2_node = graph.add_node("meio-2", 1.0, 1.0).unwrap();
        let park_node = graph.add_node("vaga-1", 2.0, 0.0).unwrap();

        graph.add_edge(in_node, mid1_node, 10, true);
        graph.add_edge(in_node, mid2_node, 15, true);
        graph.add_edge(mid1_node, park_node, 10, true);
        graph.add_edge(mid2_node, park_node, 15, true);

        graph
    }

    #[test]
    fn new_graph_is_empty() {
        let graph = ParkingGraph::new();
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.is_empty());
        assert!(graph.name_to_id.is_empty());
        assert_eq!(graph.next_id, 0);
    }

    #[test]
    fn shortest_path_uses_edge_cost_even_when_coordinates_are_misleading() {
        let mut graph = ParkingGraph::new();
        let start = graph.add_node("start", 0.0, 0.0).unwrap();
        let waypoint = graph.add_node("waypoint", 100.0, 100.0).unwrap();
        let goal = graph.add_node("goal", 1.0, 0.0).unwrap();
        graph.add_edge(start, goal, 10, false);
        graph.add_edge(start, waypoint, 1, false);
        graph.add_edge(waypoint, goal, 1, false);

        assert_eq!(graph.calculate_cost("start", "goal"), Some(2));
        assert_eq!(
            graph.calculate_route("start", "goal"),
            Some(vec!["start".into(), "waypoint".into(), "goal".into()])
        );
    }

    #[test]
    fn add_node_increments_id() {
        let mut graph = ParkingGraph::new();
        let n1 = graph.add_node("a", 0.0, 0.0).unwrap();
        let n2 = graph.add_node("b", 1.0, 1.0).unwrap();
        assert_eq!(n1, NodeId(0));
        assert_eq!(n2, NodeId(1));
    }

    #[test]
    fn add_node_stores_name_and_coords() {
        let mut graph = ParkingGraph::new();
        let id = graph.add_node("test-node", 3.5, 7.2).unwrap();
        let node = graph.nodes.get(&id).unwrap();
        assert_eq!(node.name, "test-node");
        assert_eq!(node.x, 3.5);
        assert_eq!(node.y, 7.2);
    }

    #[test]
    fn add_edge_bidirectional_creates_both_directions() {
        let mut graph = ParkingGraph::new();
        let a = graph.add_node("a", 0.0, 0.0).unwrap();
        let b = graph.add_node("b", 1.0, 0.0).unwrap();
        graph.add_edge(a, b, 10, true);

        let a_neighbors = graph.edges.get(&a).unwrap();
        assert_eq!(a_neighbors.len(), 1);
        assert_eq!(a_neighbors[0].target, b);

        let b_neighbors = graph.edges.get(&b).unwrap();
        assert_eq!(b_neighbors.len(), 1);
        assert_eq!(b_neighbors[0].target, a);
    }

    #[test]
    fn add_edge_unidirectional_creates_one_direction() {
        let mut graph = ParkingGraph::new();
        let a = graph.add_node("a", 0.0, 0.0).unwrap();
        let b = graph.add_node("b", 1.0, 0.0).unwrap();
        graph.add_edge(a, b, 10, false);

        let a_neighbors = graph.edges.get(&a).unwrap();
        assert_eq!(a_neighbors.len(), 1);

        let b_neighbors = graph.edges.get(&b).unwrap();
        assert_eq!(b_neighbors.len(), 0);
    }

    #[test]
    fn route_chooses_shortest_path() {
        let graph = build_test_graph();
        let route = graph.calculate_route("entrada", "vaga-1").unwrap();
        assert_eq!(route, vec!["entrada", "meio-1", "vaga-1"]);
    }

    #[test]
    fn route_avoids_blocked_edge() {
        let mut graph = build_test_graph();

        let route1 = graph.calculate_route("entrada", "vaga-1").unwrap();
        assert_eq!(route1, vec!["entrada", "meio-1", "vaga-1"]);

        graph.set_edge_status("entrada", "meio-1", false);

        let route2 = graph.calculate_route("entrada", "vaga-1").unwrap();
        assert_eq!(route2, vec!["entrada", "meio-2", "vaga-1"]);
    }

    #[test]
    fn no_route_if_all_blocked() {
        let mut graph = build_test_graph();

        graph.set_edge_status("entrada", "meio-1", false);
        graph.set_edge_status("entrada", "meio-2", false);

        let route = graph.calculate_route("entrada", "vaga-1");
        assert!(route.is_none());
    }

    #[test]
    fn route_nonexistent_start_returns_none() {
        let graph = build_test_graph();
        let route = graph.calculate_route("fantasma", "vaga-1");
        assert!(route.is_none());
    }

    #[test]
    fn route_nonexistent_end_returns_none() {
        let graph = build_test_graph();
        let route = graph.calculate_route("entrada", "fantasma");
        assert!(route.is_none());
    }

    #[test]
    fn route_same_start_and_end() {
        let graph = build_test_graph();
        let route = graph.calculate_route("entrada", "entrada").unwrap();
        assert_eq!(route, vec!["entrada"]);
    }

    #[test]
    fn set_edge_status_reactivates_edge() {
        let mut graph = build_test_graph();

        graph.set_edge_status("entrada", "meio-1", false);
        assert!(
            graph
                .calculate_route("entrada", "vaga-1")
                .unwrap()
                .contains(&"meio-2".to_string())
        );

        graph.set_edge_status("entrada", "meio-1", true);
        let route = graph.calculate_route("entrada", "vaga-1").unwrap();
        assert_eq!(route, vec!["entrada", "meio-1", "vaga-1"]);
    }

    #[test]
    fn set_edge_status_nonexistent_nodes_does_nothing() {
        let mut graph = build_test_graph();
        graph.set_edge_status("fantasma", "outro-fantasma", false);
        let route = graph.calculate_route("entrada", "vaga-1").unwrap();
        assert_eq!(route, vec!["entrada", "meio-1", "vaga-1"]);
    }

    #[test]
    fn route_through_longer_chain() {
        let mut graph = ParkingGraph::new();
        let a = graph.add_node("a", 0.0, 0.0).unwrap();
        let b = graph.add_node("b", 1.0, 0.0).unwrap();
        let c = graph.add_node("c", 2.0, 0.0).unwrap();
        let d = graph.add_node("d", 3.0, 0.0).unwrap();
        let e = graph.add_node("e", 4.0, 0.0).unwrap();

        graph.add_edge(a, b, 1, true);
        graph.add_edge(b, c, 1, true);
        graph.add_edge(c, d, 1, true);
        graph.add_edge(d, e, 1, true);

        let route = graph.calculate_route("a", "e").unwrap();
        assert_eq!(route, vec!["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn isolated_node_has_no_route_to_others() {
        let mut graph = ParkingGraph::new();
        graph.add_node("island", 0.0, 0.0).unwrap();
        let a = graph.add_node("a", 1.0, 0.0).unwrap();
        let b = graph.add_node("b", 2.0, 0.0).unwrap();
        graph.add_edge(a, b, 10, true);

        assert!(graph.calculate_route("island", "a").is_none());
        assert!(graph.calculate_route("island", "b").is_none());
    }

    #[test]
    fn route_prefers_lower_cost_path() {
        let mut graph = ParkingGraph::new();
        let a = graph.add_node("a", 0.0, 0.0).unwrap();
        let b = graph.add_node("b", 0.0, 0.0).unwrap();
        let c = graph.add_node("c", 0.0, 0.0).unwrap();

        graph.add_edge(a, b, 100, false);
        graph.add_edge(a, c, 1, false);
        graph.add_edge(c, b, 1, false);

        let route = graph.calculate_route("a", "b").unwrap();
        assert_eq!(route, vec!["a", "c", "b"]);
    }

    #[test]
    fn duplicate_name_does_not_replace_an_existing_route_origin() {
        let mut graph = ParkingGraph::new();
        let origin = graph.add_node("cam-01", 0.0, 0.0).unwrap();
        let spot = graph.add_node("spot", 1.0, 0.0).unwrap();
        graph.add_edge(origin, spot, 1000, true);
        let error = graph.add_node("cam-01", 99.0, 99.0).unwrap_err();
        assert_eq!(error.name, "cam-01");
        assert_eq!(
            graph.calculate_route("cam-01", "spot"),
            Some(vec!["cam-01".into(), "spot".into()])
        );
        assert_eq!(graph.nodes.len(), 2);
    }
}
