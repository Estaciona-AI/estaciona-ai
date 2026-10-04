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

use serde::Deserialize;
use serde_json::Value;
use std::{
    io,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize)]
pub struct PathNode {
    pub x: f64,
    pub z: f64,
}

#[derive(Debug, Deserialize)]
pub struct ParkingLotConfig {
    pub path: Vec<PathNode>,
}

#[derive(Debug, Deserialize)]
pub struct Coords3D {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

#[derive(Debug, Deserialize)]
pub struct Spot3DDef {
    pub id: String,
    #[serde(rename = "center3D")]
    pub center_3d: Coords3D,
}

#[derive(Debug)]
pub struct SiteConfig {
    pub directory: PathBuf,
    pub parkings: Vec<ParkingLotConfig>,
    pub spots: Vec<Spot3DDef>,
}

impl SiteConfig {
    pub fn load(directory: impl Into<PathBuf>) -> io::Result<Self> {
        let directory = directory.into();
        let config_path = directory.join("config.json");
        let raw: Value = read_json(&config_path)?;
        validate_parking_config(&raw)?;
        let parkings = serde_json::from_value(raw).map_err(invalid_data)?;
        let spots: Vec<Spot3DDef> = read_json(&directory.join("spots_3d.json"))?;
        let mut ids = std::collections::HashSet::new();
        for spot in &spots {
            if spot.id.trim().is_empty() || !ids.insert(&spot.id) {
                return Err(invalid_data("Empty or duplicate spot ID in spots_3d.json"));
            }
            if ![spot.center_3d.x, spot.center_3d.y, spot.center_3d.z]
                .iter()
                .all(|v| v.is_finite())
            {
                return Err(invalid_data("Non-finite spot coordinates in spots_3d.json"));
            }
        }
        Ok(Self {
            directory,
            parkings,
            spots,
        })
    }
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
    serde_json::from_str(&content)
        .map_err(|error| invalid_data(format!("{}: {error}", path.display())))
}

fn validate_parking_config(payload: &Value) -> io::Result<()> {
    let lots = payload
        .as_array()
        .ok_or_else(|| invalid_data("Expected parking lots array"))?;
    if lots.is_empty()
        || lots[0]
            .get("path")
            .and_then(Value::as_array)
            .is_none_or(|p| p.len() < 2)
    {
        return Err(invalid_data(
            "First parking lot needs at least two path points",
        ));
    }
    for lot in lots {
        let valid_name = lot.get("name").and_then(Value::as_str).is_some_and(|name| {
            !name.is_empty() && name.len() <= 100 && !name.contains(['<', '>', '&'])
        });
        let valid_path = lot
            .get("path")
            .and_then(Value::as_array)
            .is_some_and(|path| {
                path.iter().all(|point| {
                    ["x", "z"].iter().all(|axis| {
                        point
                            .get(axis)
                            .and_then(Value::as_f64)
                            .is_some_and(f64::is_finite)
                    })
                })
            });
        if !valid_name || !valid_path {
            return Err(invalid_data("Invalid parking lot configuration"));
        }
    }
    Ok(())
}

pub fn save_parking_config(directory: &Path, payload: &Value) -> io::Result<()> {
    validate_parking_config(payload)?;
    let content = serde_json::to_string_pretty(payload).map_err(invalid_data)?;
    std::fs::write(directory.join("config.json"), content)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let directory =
                std::env::temp_dir().join(format!("estaciona-site-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            Self(directory)
        }

        fn write(&self, file: &str, value: &Value) {
            std::fs::write(self.0.join(file), serde_json::to_vec(value).unwrap()).unwrap();
        }

        fn seed(&self) {
            self.write("config.json", &serde_json::json!([
                {"name": "Example", "camera": {"example": true}, "path": [{"x": 0, "z": 0}, {"x": 4, "z": 0}]},
                {"name": "Optional", "path": []}
            ]));
            self.write(
                "spots_3d.json",
                &serde_json::json!([
                    {"id": "EX-01", "center3D": {"x": 1, "y": 0, "z": 1}, "polygonPixels": []},
                    {"id": "EX-02", "center3D": {"x": 3, "y": 0, "z": 1}}
                ]),
            );
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn loads_external_site_maps_and_preserves_existing_json_format() {
        let fixture = Fixture::new();
        fixture.seed();
        let site = SiteConfig::load(&fixture.0).unwrap();
        assert_eq!(site.directory, fixture.0);
        assert_eq!(site.parkings.len(), 2);
        assert_eq!(site.parkings[0].path[1].x, 4.0);
        assert_eq!(site.parkings[0].path[1].z, 0.0);
        assert_eq!(site.spots[0].id, "EX-01");
        assert_eq!(site.spots[1].center_3d.x, 3.0);
        assert_eq!(site.spots[1].center_3d.y, 0.0);
        assert_eq!(site.spots[1].center_3d.z, 1.0);
    }

    #[test]
    fn missing_map_reports_the_external_filename() {
        let fixture = Fixture::new();
        let error = SiteConfig::load(&fixture.0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("config.json"));
    }

    #[test]
    fn rejects_missing_path_and_duplicate_spot_ids() {
        let fixture = Fixture::new();
        fixture.seed();
        fixture.write(
            "config.json",
            &serde_json::json!([{"name": "Example", "path": []}]),
        );
        assert_eq!(
            SiteConfig::load(&fixture.0).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        fixture.seed();
        fixture.write(
            "spots_3d.json",
            &serde_json::json!([
                {"id": "EX-01", "center3D": {"x": 1, "y": 0, "z": 1}},
                {"id": "EX-01", "center3D": {"x": 3, "y": 0, "z": 1}}
            ]),
        );
        assert_eq!(
            SiteConfig::load(&fixture.0).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn persists_only_to_selected_site_and_keeps_extra_fields() {
        let target = Fixture::new();
        target.seed();
        let other = Fixture::new();
        other.seed();
        let unchanged = std::fs::read(other.0.join("config.json")).unwrap();
        let payload = serde_json::json!([
            {"name": "Updated", "camera": {"position": {"x": 5}}, "path": [{"x": 0, "z": 0}, {"x": 4, "z": 0}]}
        ]);
        save_parking_config(&target.0, &payload).unwrap();
        let stored: Value =
            serde_json::from_slice(&std::fs::read(target.0.join("config.json")).unwrap()).unwrap();
        assert_eq!(stored, payload);
        assert_eq!(
            std::fs::read(other.0.join("config.json")).unwrap(),
            unchanged
        );
    }

    #[test]
    fn invalid_updates_do_not_replace_existing_config() {
        let fixture = Fixture::new();
        fixture.seed();
        let original = std::fs::read(fixture.0.join("config.json")).unwrap();
        let invalid =
            serde_json::json!([{"name": "<script>", "path": [{"x": 0, "z": 0}, {"x": 1, "z": 0}]}]);
        assert_eq!(
            save_parking_config(&fixture.0, &invalid)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            std::fs::read(fixture.0.join("config.json")).unwrap(),
            original
        );
    }
}
