// SPDX-FileCopyrightText: 2026 Guilherme Pedroza
// SPDX-License-Identifier: Apache-2.0

use serde::Deserialize;
use serde_json::Value;
use std::{
    io::{self, Write},
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
        let parkings: Vec<ParkingLotConfig> = serde_json::from_value(raw).map_err(invalid_data)?;
        let spots: Vec<Spot3DDef> = read_json(&directory.join("spots_3d.json"))?;
        validate_spots(&spots)?;
        crate::site_graph::validate_names(&parkings, &spots).map_err(invalid_data)?;
        Ok(Self {
            directory,
            parkings,
            spots,
        })
    }
}

fn validate_spots(spots: &[Spot3DDef]) -> io::Result<()> {
    let mut ids = std::collections::HashSet::new();
    for spot in spots {
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
    Ok(())
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
    let parkings = Vec::<ParkingLotConfig>::deserialize(payload).map_err(invalid_data)?;
    // Stored geometry failures are infrastructure errors, not invalid requests.
    let spots: Vec<Spot3DDef> =
        read_json(&directory.join("spots_3d.json")).map_err(io::Error::other)?;
    validate_spots(&spots).map_err(io::Error::other)?;
    crate::site_graph::validate_names(&parkings, &spots).map_err(invalid_data)?;
    let content = serde_json::to_string_pretty(payload).map_err(invalid_data)?;
    replace_config_file(directory, |file| file.write_all(content.as_bytes()))
}

// A local-file adapter: the caller owns the destination and write admission.
// Readers see either complete version; the active file is never truncated.
fn replace_config_file(
    directory: &Path,
    write_content: impl FnOnce(&mut std::fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = std::fs::File::open(directory)?;
    let destination = directory.join("config.json");
    let permissions = match std::fs::metadata(&destination) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let path = directory.join(format!(".config-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(&path)?;
    let mut pending = PendingConfigFile { path, file };
    write_content(&mut pending.file)?;
    if let Some(permissions) = permissions {
        pending.file.set_permissions(permissions)?;
    }
    pending.file.sync_all()?;
    std::fs::rename(&pending.path, destination)?;
    // A failure here is reported, but replacement has already happened.
    // It does not justify rolling back a concurrently observed version.
    parent.sync_all()
}

struct PendingConfigFile {
    path: PathBuf,
    file: std::fs::File,
}

impl Drop for PendingConfigFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(%error, "Failed to clean up temporary configuration file");
        }
    }
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

    #[test]
    fn r10_replacement_preserves_the_previous_file_inode() {
        let fixture = Fixture::new();
        fixture.seed();
        let old_path = fixture.0.join("previous-config.json");
        std::fs::hard_link(fixture.0.join("config.json"), &old_path).unwrap();
        let original = std::fs::read(&old_path).unwrap();
        let payload = serde_json::json!([
            {"name": "Replacement", "path": [{"x": 0, "z": 0}, {"x": 8, "z": 0}]}
        ]);
        save_parking_config(&fixture.0, &payload).unwrap();
        assert_eq!(std::fs::read(old_path).unwrap(), original);
        assert_eq!(
            read_json::<Value>(&fixture.0.join("config.json")).unwrap(),
            payload
        );
    }

    fn assert_no_pending_files(directory: &Path) {
        assert!(std::fs::read_dir(directory).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn r10_partial_write_failure_keeps_config_and_cleans_temporary_file() {
        let fixture = Fixture::new();
        fixture.seed();
        let original = std::fs::read(fixture.0.join("config.json")).unwrap();
        let result = replace_config_file(&fixture.0, |file| {
            file.write_all(b"partial json")?;
            Err(io::Error::other("injected disk write failure"))
        });
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(fixture.0.join("config.json")).unwrap(),
            original
        );
        assert_no_pending_files(&fixture.0);
    }

    #[test]
    fn r10_readers_keep_complete_config_until_publication() {
        let fixture = Fixture::new();
        fixture.seed();
        let original = std::fs::read(fixture.0.join("config.json")).unwrap();
        replace_config_file(&fixture.0, |file| {
            file.write_all(b"[{")?;
            assert_eq!(std::fs::read(fixture.0.join("config.json"))?, original);
            file.write_all(b"}]")
        })
        .unwrap();
        assert_eq!(
            std::fs::read(fixture.0.join("config.json")).unwrap(),
            b"[{}]"
        );
        assert_no_pending_files(&fixture.0);
    }

    #[test]
    fn r10_rename_failure_cleans_temporary_file() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.0.join("config.json")).unwrap();
        let result = replace_config_file(&fixture.0, |file| file.write_all(b"[]"));
        assert!(result.is_err());
        assert!(fixture.0.join("config.json").is_dir());
        assert_no_pending_files(&fixture.0);
    }

    #[test]
    fn r10_writer_panic_keeps_config_and_cleans_temporary_file() {
        let fixture = Fixture::new();
        fixture.seed();
        let original = std::fs::read(fixture.0.join("config.json")).unwrap();
        let result = std::panic::catch_unwind(|| {
            let _ = replace_config_file(&fixture.0, |file| {
                file.write_all(b"partial")?;
                panic!("injected writer panic");
            });
        });
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(fixture.0.join("config.json")).unwrap(),
            original
        );
        assert_no_pending_files(&fixture.0);
    }

    #[cfg(unix)]
    #[test]
    fn r10_atomic_replacement_preserves_existing_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        fixture.seed();
        let path = fixture.0.join("config.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        replace_config_file(&fixture.0, |file| file.write_all(b"[]")).unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn r11_loading_rejects_collisions_with_generated_navigation_names() {
        let fixture = Fixture::new();
        fixture.seed();
        for id in ["cam-01", "path_node_1"] {
            fixture.write(
                "spots_3d.json",
                &serde_json::json!([
                    {"id": id, "center3D": {"x": 1, "y": 0, "z": 1}}
                ]),
            );
            let error = SiteConfig::load(&fixture.0).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert!(error.to_string().contains(id));
        }
    }

    #[test]
    fn r11_saving_rejects_new_collision_without_replacing_the_valid_config() {
        let fixture = Fixture::new();
        fixture.seed();
        fixture.write(
            "config.json",
            &serde_json::json!([
                {"name": "Short", "path": [{"x": 0, "z": 0}, {"x": 0, "z": 0}]}
            ]),
        );
        fixture.write(
            "spots_3d.json",
            &serde_json::json!([
                {"id": "path_node_1", "center3D": {"x": 1, "y": 0, "z": 1}}
            ]),
        );
        SiteConfig::load(&fixture.0).unwrap();
        let before = std::fs::read(fixture.0.join("config.json")).unwrap();
        let payload = serde_json::json!([
            {"name": "Extended", "path": [{"x": 0, "z": 0}, {"x": 1, "z": 0}]}
        ]);
        let error = save_parking_config(&fixture.0, &payload).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("path_node_1"));
        assert_eq!(
            std::fs::read(fixture.0.join("config.json")).unwrap(),
            before
        );
        assert_no_pending_files(&fixture.0);
    }
}
