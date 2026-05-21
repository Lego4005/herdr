use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub fn load_favorites(workspace_root: &Path) -> HashSet<PathBuf> {
    let config_dir = super::config_dir();
    let fav_path = config_dir.join("favorites.json");
    if !fav_path.exists() {
        return HashSet::new();
    }

    let Ok(content) = std::fs::read_to_string(&fav_path) else {
        return HashSet::new();
    };

    let Ok(map): Result<HashMap<String, Vec<PathBuf>>, _> = serde_json::from_str(&content) else {
        return HashSet::new();
    };

    let key = workspace_root.to_string_lossy().into_owned();
    if let Some(list) = map.get(&key) {
        list.iter().cloned().collect()
    } else {
        HashSet::new()
    }
}

pub fn save_favorite(workspace_root: &Path, file_path: &Path, is_favorite: bool) {
    let config_dir = super::config_dir();
    // Ensure directory exists
    let _ = std::fs::create_dir_all(&config_dir);
    let fav_path = config_dir.join("favorites.json");

    let mut map: HashMap<String, Vec<PathBuf>> = if fav_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&fav_path) {
            serde_json::from_str(&content).unwrap_or_default()
        } else {
            HashMap::new()
        }
    } else {
        HashMap::new()
    };

    let key = workspace_root.to_string_lossy().into_owned();
    let mut current_favs: HashSet<PathBuf> = map
        .get(&key)
        .map(|v| v.iter().cloned().collect())
        .unwrap_or_default();

    if is_favorite {
        current_favs.insert(file_path.to_path_buf());
    } else {
        current_favs.remove(file_path);
    }

    let mut new_list: Vec<PathBuf> = current_favs.into_iter().collect();
    new_list.sort(); // keep it sorted deterministically in JSON
    map.insert(key, new_list);

    if let Ok(json_str) = serde_json::to_string_pretty(&map) {
        let _ = std::fs::write(&fav_path, json_str);
    }
}
