//! Reading (and carefully editing) saved player data: `<world>/playerdata/<uuid>.dat`,
//! gzip-compressed NBT. Names come from the server's `usercache.json`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use fastnbt::Value;
use serde::Serialize;

#[derive(Serialize)]
pub struct PlayerSummary {
    pub uuid: String,
    pub name: Option<String>,
    /// When the player file was last written (roughly: last logout or autosave)
    pub last_saved: i64,
    pub dimension: Option<String>,
    pub pos: Option<[f64; 3]>,
}

#[derive(Serialize)]
pub struct Item {
    pub slot: i32,
    pub id: String,
    pub count: i32,
    pub custom_name: Option<String>,
    pub enchanted: bool,
}

#[derive(Serialize)]
pub struct PlayerData {
    #[serde(flatten)]
    pub summary: PlayerSummary,
    pub health: Option<f32>,
    pub xp_level: Option<i32>,
    pub selected_slot: Option<i32>,
    pub inventory: Vec<Item>,
    pub ender_chest: Vec<Item>,
}

// ── Files ─────────────────────────────────────────────────────────────────────

fn world_dir(server_path: &Path) -> PathBuf {
    server_path.join(crate::datapack_mgr::read_level_name(server_path).unwrap_or_else(|| "world".to_string()))
}

fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12]) && groups.iter().all(|g| g.chars().all(|c| c.is_ascii_hexdigit()))
}

fn player_file(server_path: &Path, uuid: &str) -> Result<PathBuf, String> {
    if !is_uuid(uuid) {
        return Err("Invalid player UUID".to_string());
    }
    let path = world_dir(server_path).join("playerdata").join(format!("{}.dat", uuid));
    if path.is_file() { Ok(path) } else { Err("No saved data for that player".to_string()) }
}

fn read_nbt(path: &Path) -> Result<Value, String> {
    let raw = std::fs::read(path).map_err(|e| format!("Can't read {}: {}", path.display(), e))?;
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(&raw[..])
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{} isn't gzip-compressed NBT: {}", path.display(), e))?;
    fastnbt::from_bytes(&bytes).map_err(|e| format!("Can't parse {}: {}", path.display(), e))
}

/// Writes NBT back via a temp file + rename, keeping the previous file as `<name>.msm-bak`.
fn write_nbt(path: &Path, value: &Value) -> Result<(), String> {
    let bytes = fastnbt::to_bytes(value).map_err(|e| format!("Can't encode player data: {}", e))?;
    let compressed = {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&bytes).map_err(|e| e.to_string())?;
        gz.finish().map_err(|e| e.to_string())?
    };

    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    std::fs::copy(path, path.with_file_name(format!("{}.msm-bak", name)))
        .map_err(|e| format!("Couldn't back up the player file: {}", e))?;
    let tmp = path.with_file_name(format!(".{}.msm-tmp", name));
    std::fs::write(&tmp, compressed).map_err(|e| format!("Failed to save: {}", e))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("Failed to save: {}", e)
    })
}

fn usercache(server_path: &Path) -> HashMap<String, String> {
    #[derive(serde::Deserialize)]
    struct Entry { name: String, uuid: String }
    std::fs::read_to_string(server_path.join("usercache.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<Entry>>(&s).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|e| (e.uuid.to_lowercase(), e.name))
        .collect()
}

// ── NBT helpers ───────────────────────────────────────────────────────────────

fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v { Value::Compound(m) => m.get(key), _ => None }
}

fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Byte(n) => Some(*n as i64),
        Value::Short(n) => Some(*n as i64),
        Value::Int(n) => Some(*n as i64),
        Value::Long(n) => Some(*n),
        _ => None,
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float(n) => Some(*n as f64),
        Value::Double(n) => Some(*n),
        other => as_i64(other).map(|n| n as f64),
    }
}

fn pos_of(root: &Value) -> Option<[f64; 3]> {
    match get(root, "Pos")? {
        Value::List(l) if l.len() == 3 => Some([as_f64(&l[0])?, as_f64(&l[1])?, as_f64(&l[2])?]),
        _ => None,
    }
}

fn dimension_of(root: &Value) -> Option<String> {
    match get(root, "Dimension")? {
        Value::String(s) => Some(s.clone()),
        // Before 1.16 dimensions were numbers
        other => Some(match as_i64(other)? {
            -1 => "minecraft:the_nether",
            1 => "minecraft:the_end",
            _ => "minecraft:overworld",
        }.to_string()),
    }
}

/// Plain text of a custom item name, stored as JSON text (≤1.21.4) or an NBT text component.
fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(serde_json::Value::String(t)) => Some(t),
            Ok(obj @ serde_json::Value::Object(_)) => obj.get("text").and_then(|t| t.as_str()).map(String::from),
            _ => Some(s.clone()),
        },
        Value::Compound(_) => match get(v, "text")? { Value::String(t) => Some(t.clone()), _ => None },
        _ => None,
    }
}

/// Armor and off-hand. Before 1.21.5 they were Inventory slots 100–103 and -106; newer versions
/// keep them in an `equipment` compound, mapped here onto the old slot numbers.
fn equipment_of(root: &Value) -> Vec<Item> {
    let Some(Value::Compound(eq)) = get(root, "equipment") else { return Vec::new() };
    let slot_of = |name: &str| match name {
        "feet" => Some(100), "legs" => Some(101), "chest" => Some(102), "head" => Some(103), "offhand" => Some(-106),
        _ => None,
    };
    eq.iter()
        .filter_map(|(name, item)| {
            let mut parsed = parse_item(item)?;
            parsed.slot = slot_of(name)?;
            Some(parsed)
        })
        .collect()
}

fn items_of(root: &Value, key: &str) -> Vec<Item> {
    let Some(Value::List(list)) = get(root, key) else { return Vec::new() };
    list.iter().filter_map(parse_item).collect()
}

fn parse_item(item: &Value) -> Option<Item> {
    let non_empty = |v: Option<&Value>| match v {
        Some(Value::List(l)) => !l.is_empty(),
        Some(Value::Compound(m)) => match m.get("levels") {
            Some(Value::Compound(levels)) => !levels.is_empty(),
            _ => !m.is_empty(),
        },
        _ => false,
    };
    let id = match get(item, "id")? { Value::String(s) => s.clone(), _ => return None };
    // 1.20.5+ uses `count` and `components`; older versions `Count` and `tag`
    let count = get(item, "count").or_else(|| get(item, "Count")).and_then(as_i64).unwrap_or(1) as i32;
    let components = get(item, "components");
    let tag = get(item, "tag");
    let custom_name = components
        .and_then(|c| get(c, "minecraft:custom_name"))
        .or_else(|| tag.and_then(|t| get(t, "display")).and_then(|d| get(d, "Name")))
        .and_then(text_of);
    let enchanted = components.is_some_and(|c| {
        non_empty(get(c, "minecraft:enchantments")) || non_empty(get(c, "minecraft:stored_enchantments"))
    }) || tag.is_some_and(|t| non_empty(get(t, "Enchantments")) || non_empty(get(t, "StoredEnchantments")));
    Some(Item {
        slot: get(item, "Slot").and_then(as_i64).unwrap_or(-1) as i32,
        id,
        count,
        custom_name,
        enchanted,
    })
}

fn summary(uuid: &str, name: Option<String>, path: &Path, root: &Value) -> PlayerSummary {
    let last_saved = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    PlayerSummary { uuid: uuid.to_string(), name, last_saved, dimension: dimension_of(root), pos: pos_of(root) }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Every player with saved data, most recently saved first.
pub fn list(server_path: &Path) -> Result<Vec<PlayerSummary>, String> {
    let dir = world_dir(server_path).join("playerdata");
    let names = usercache(server_path);
    let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(Vec::new()) };
    let mut players: Vec<PlayerSummary> = entries
        .flatten()
        .filter_map(|e| {
            let file = e.file_name().to_string_lossy().into_owned();
            let uuid = file.strip_suffix(".dat")?.to_lowercase();
            if !is_uuid(&uuid) { return None; }
            let root = read_nbt(&e.path()).ok()?;
            Some(summary(&uuid, names.get(&uuid).cloned(), &e.path(), &root))
        })
        .collect();
    players.sort_by(|a, b| b.last_saved.cmp(&a.last_saved));
    Ok(players)
}

pub fn read(server_path: &Path, uuid: &str) -> Result<PlayerData, String> {
    let uuid = uuid.to_lowercase();
    let path = player_file(server_path, &uuid)?;
    let root = read_nbt(&path)?;
    Ok(PlayerData {
        summary: summary(&uuid, usercache(server_path).get(&uuid).cloned(), &path, &root),
        health: get(&root, "Health").and_then(as_f64).map(|h| h as f32),
        xp_level: get(&root, "XpLevel").and_then(as_i64).map(|n| n as i32),
        selected_slot: get(&root, "SelectedItemSlot").and_then(as_i64).map(|n| n as i32),
        inventory: items_of(&root, "Inventory").into_iter().chain(equipment_of(&root)).collect(),
        ender_chest: items_of(&root, "EnderItems"),
    })
}

/// The world spawn from level.dat: `Data.SpawnX/Y/Z`, or `Data.spawn.pos` in newer versions.
fn world_spawn(server_path: &Path) -> Result<[i32; 3], String> {
    let level = read_nbt(&world_dir(server_path).join("level.dat"))?;
    let data = get(&level, "Data").ok_or("level.dat has no Data section")?;
    let xyz = |k: &str| get(data, k).and_then(as_i64).map(|n| n as i32);
    if let (Some(x), Some(y), Some(z)) = (xyz("SpawnX"), xyz("SpawnY"), xyz("SpawnZ")) {
        return Ok([x, y, z]);
    }
    match get(data, "spawn").and_then(|s| get(s, "pos")) {
        Some(Value::IntArray(a)) if a.len() == 3 => Ok([a[0], a[1], a[2]]),
        _ => Err("Couldn't find the world spawn in level.dat".to_string()),
    }
}

/// Moves a player to the world spawn in the overworld. Only safe while the server is stopped,
/// since a running server overwrites player files.
pub fn move_to_spawn(server_path: &Path, uuid: &str) -> Result<[f64; 3], String> {
    let uuid = uuid.to_lowercase();
    let path = player_file(server_path, &uuid)?;
    let [x, y, z] = world_spawn(server_path)?;
    let pos = [x as f64 + 0.5, y as f64, z as f64 + 0.5];

    let mut root = read_nbt(&path)?;
    let Value::Compound(map) = &mut root else { return Err("Player data isn't an NBT compound".to_string()) };
    map.insert("Pos".to_string(), Value::List(pos.iter().map(|c| Value::Double(*c)).collect()));
    map.insert("Motion".to_string(), Value::List(vec![Value::Double(0.0); 3]));
    map.insert("Dimension".to_string(), Value::String("minecraft:overworld".to_string()));
    for key in ["FallDistance", "fall_distance"] {
        if map.contains_key(key) {
            map.insert(key.to_string(), if key == "FallDistance" { Value::Float(0.0) } else { Value::Double(0.0) });
        }
    }
    write_nbt(&path, &root)?;
    Ok(pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_server() -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("msm-players-test-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(dir.join("world/playerdata")).unwrap();
        dir
    }

    fn write_gz(path: &Path, v: &Value) {
        let bytes = fastnbt::to_bytes(v).unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&bytes).unwrap();
        std::fs::write(path, gz.finish().unwrap()).unwrap();
    }

    fn compound(pairs: Vec<(&str, Value)>) -> Value {
        Value::Compound(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    const UUID: &str = "069a79f4-44e9-4726-a5be-fca90e38aaf5";

    fn setup() -> PathBuf {
        let s = temp_server();
        std::fs::write(s.join("usercache.json"), format!(r#"[{{"name":"Notch","uuid":"{UUID}","expiresOn":"x"}}]"#)).unwrap();
        let player = compound(vec![
            ("Pos", Value::List(vec![Value::Double(100.5), Value::Double(-40.0), Value::Double(7.25)])),
            ("Dimension", Value::String("minecraft:the_nether".into())),
            ("Health", Value::Float(12.0)),
            ("XpLevel", Value::Int(30)),
            ("SomeModData", Value::LongArray(fastnbt::LongArray::new(vec![1, 2, 3]))),
            ("Inventory", Value::List(vec![
                compound(vec![("Slot", Value::Byte(0)), ("id", Value::String("minecraft:diamond_sword".into())), ("count", Value::Int(1)),
                    ("components", compound(vec![
                        ("minecraft:custom_name", Value::String(r#"{"text":"Slicer"}"#.into())),
                        ("minecraft:enchantments", compound(vec![("levels", compound(vec![("minecraft:sharpness", Value::Int(5))]))])),
                    ]))]),
                compound(vec![("Slot", Value::Byte(1)), ("id", Value::String("create:cogwheel".into())), ("Count", Value::Byte(64))]),
            ])),
            ("EnderItems", Value::List(vec![])),
        ]);
        write_gz(&s.join(format!("world/playerdata/{UUID}.dat")), &player);
        let level = compound(vec![("Data", compound(vec![("SpawnX", Value::Int(10)), ("SpawnY", Value::Int(64)), ("SpawnZ", Value::Int(-20))]))]);
        write_gz(&s.join("world/level.dat"), &level);
        s
    }

    #[test]
    fn lists_and_reads_players() {
        let s = setup();
        let players = list(&s).unwrap();
        assert_eq!(players.len(), 1);
        assert_eq!(players[0].name.as_deref(), Some("Notch"));
        assert_eq!(players[0].dimension.as_deref(), Some("minecraft:the_nether"));

        let p = read(&s, UUID).unwrap();
        assert_eq!(p.health, Some(12.0));
        assert_eq!(p.xp_level, Some(30));
        assert_eq!(p.inventory.len(), 2);
        let sword = &p.inventory[0];
        assert_eq!((sword.id.as_str(), sword.count, sword.custom_name.as_deref(), sword.enchanted),
                   ("minecraft:diamond_sword", 1, Some("Slicer"), true));
        let cog = &p.inventory[1];
        assert_eq!((cog.id.as_str(), cog.count, cog.enchanted), ("create:cogwheel", 64, false));
    }

    #[test]
    fn moves_player_to_spawn_and_keeps_other_data() {
        let s = setup();
        assert_eq!(move_to_spawn(&s, UUID).unwrap(), [10.5, 64.0, -19.5]);
        let root = read_nbt(&s.join(format!("world/playerdata/{UUID}.dat"))).unwrap();
        assert_eq!(pos_of(&root), Some([10.5, 64.0, -19.5]));
        assert_eq!(dimension_of(&root).as_deref(), Some("minecraft:overworld"));
        // Unrelated data survives the round trip, including array tags
        assert!(matches!(get(&root, "SomeModData"), Some(Value::LongArray(a)) if a.len() == 3));
        assert_eq!(items_of(&root, "Inventory").len(), 2);
        assert!(s.join(format!("world/playerdata/{UUID}.dat.msm-bak")).exists());
    }

    #[test]
    fn reads_1_21_5_equipment() {
        let root = compound(vec![("equipment", compound(vec![
            ("head", compound(vec![("id", Value::String("minecraft:netherite_helmet".into())), ("count", Value::Int(1))])),
            ("offhand", compound(vec![("id", Value::String("minecraft:shield".into())), ("count", Value::Int(1))])),
        ]))]);
        let mut slots: Vec<(i32, String)> = equipment_of(&root).into_iter().map(|i| (i.slot, i.id)).collect();
        slots.sort();
        assert_eq!(slots, vec![(-106, "minecraft:shield".to_string()), (103, "minecraft:netherite_helmet".to_string())]);
    }

    #[test]
    fn refuses_bad_uuids() {
        let s = setup();
        assert!(read(&s, "../../level").is_err());
        assert!(move_to_spawn(&s, "not-a-uuid").is_err());
    }
}
