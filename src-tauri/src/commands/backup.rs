//! Native backup/restore for the local chat database.
//!
//! Replaces the old all-JS export (one giant JSON file with photos/videos
//! base64-embedded inline in the rows) with a zip archive built directly in
//! Rust: SQL reads, base64 decode, and compression all happen natively
//! instead of crossing the JS<->WebView IPC bridge as multi-hundred-MB text.
//!
//! Archive layout (version 2):
//!   manifest.json        - { version, exportedAt, ownerUserId, categories }
//!   tables/<name>.jsonl   - one JSON object per row, ORDER BY the table's primary key
//!                           (only present for tables whose category was selected)
//!   media/<n>.bin         - raw bytes for each blob column, referenced from its
//!                           row as { "$mediaRef": "media/<n>.bin" } instead of
//!                           inlining base64 text
//!
//! `categories` lets the caller include/exclude whole groups of tables
//! (chat messages, media/albums, saved phrases, saved locations, settings,
//! sexual health - see `BACKUP_CATEGORY_IDS` in `src/services/backupRestore.ts`)
//! instead of always doing a full all-or-nothing export/import.
//!
//! Blob columns (data_base64 etc.) are stored as base64 TEXT in the sqlite
//! schema itself, so export decodes them once here and stores raw bytes
//! (Stored, uncompressed - photos/video are already compressed); import
//! re-encodes them back to base64 text to match the existing column type.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::Path;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::{json, Map, Value};
use tauri::{AppHandle, Emitter, Manager};
use zip::result::ZipError;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::error::AppError;

const BACKUP_VERSION: i64 = 2;
const EXPORT_PROGRESS_EVENT: &str = "backup-export-progress";
const IMPORT_PROGRESS_EVENT: &str = "backup-import-progress";
const PROGRESS_EVERY_N_ROWS: u64 = 200;

struct TableSpec {
    name: &'static str,
    primary_key: &'static str,
    /// One of the user-facing backup categories (see `BACKUP_CATEGORIES` in
    /// `src/services/backupRestore.ts`) - lets export/import include or skip
    /// whole groups of tables together instead of always doing all-or-nothing.
    category: &'static str,
    columns: &'static [&'static str],
    blob_columns: &'static [&'static str],
}

/// Mirrors `FULL_EXPORT_TABLES` in `src/services/chatDb.ts` exactly, including
/// the deliberate omission of `album_media.position` (a real schema column
/// that the JS export has never included) - keep behavior identical.
const EXPORT_TABLES: &[TableSpec] = &[
    TableSpec {
        name: "conversations",
        primary_key: "conversation_id",
        category: "chat_messages",
        columns: &[
            "conversation_id",
            "other_profile_id",
            "name",
            "participants_json",
            "last_activity_timestamp",
            "unread_count",
            "pinned",
            "muted",
            "favorite",
            "preview_json",
            "archived",
            "archived_reason",
            "archived_at",
            "hidden",
            "last_seen_in_inbox_at",
            "created_at",
            "updated_at",
        ],
        blob_columns: &[],
    },
    TableSpec {
        name: "conversation_meta",
        primary_key: "conversation_id",
        category: "chat_messages",
        columns: &["conversation_id", "last_read_timestamp"],
        blob_columns: &[],
    },
    TableSpec {
        name: "messages",
        primary_key: "message_id",
        category: "chat_messages",
        columns: &[
            "message_id",
            "conversation_id",
            "sender_id",
            "timestamp",
            "type",
            "chat1_type",
            "body_json",
            "unsent",
            "local_history",
            "reply_to_message_id",
            "reply_preview_json",
            "reactions_json",
            "created_at",
            "updated_at",
        ],
        blob_columns: &[],
    },
    TableSpec {
        name: "media_files",
        primary_key: "media_key",
        category: "media_albums",
        columns: &[
            "media_key",
            "conversation_id",
            "message_id",
            "kind",
            "mime_type",
            "data_base64",
            "view_once",
            "size_bytes",
            "fetch_status",
            "fetched_at",
        ],
        blob_columns: &["data_base64"],
    },
    TableSpec {
        name: "albums",
        primary_key: "album_id",
        category: "media_albums",
        columns: &[
            "album_id",
            "owner_profile_id",
            "album_name",
            "conversation_id",
            "shared_via_message_id",
            "preview_cover_base64",
            "preview_cover_mime_type",
            "created_at",
            "updated_at",
        ],
        blob_columns: &["preview_cover_base64"],
    },
    TableSpec {
        name: "album_media",
        primary_key: "content_id",
        category: "media_albums",
        columns: &[
            "content_id",
            "album_id",
            "content_type",
            "data_base64",
            "thumb_data_base64",
            "remaining_views",
            "is_viewable",
            "fetched_at",
        ],
        blob_columns: &["data_base64", "thumb_data_base64"],
    },
    TableSpec {
        name: "avatars",
        primary_key: "media_hash",
        category: "media_albums",
        columns: &["media_hash", "data_base64", "mime_type", "fetched_at"],
        blob_columns: &["data_base64"],
    },
    TableSpec {
        name: "settings",
        primary_key: "key",
        category: "settings",
        columns: &["key", "value"],
        blob_columns: &[],
    },
    TableSpec {
        name: "saved_phrases",
        primary_key: "phrase",
        category: "saved_phrases",
        columns: &["phrase", "created_at"],
        blob_columns: &[],
    },
    TableSpec {
        name: "saved_locations",
        primary_key: "id",
        category: "saved_locations",
        columns: &["id", "name", "geohash", "lat", "lon", "created_at"],
        blob_columns: &[],
    },
    TableSpec {
        name: "sexual_health_prep_doses",
        primary_key: "id",
        category: "sexual_health",
        columns: &["id", "taken_at", "scheme", "dose_role", "note", "created_at"],
        blob_columns: &[],
    },
    TableSpec {
        name: "sexual_health_encounters",
        primary_key: "id",
        category: "sexual_health",
        columns: &[
            "id",
            "occurred_at",
            "profile_id",
            "display_name",
            "tags_json",
            "note",
            "conversation_id",
            "created_at",
        ],
        blob_columns: &[],
    },
    TableSpec {
        name: "sexual_health_appointments",
        primary_key: "id",
        category: "sexual_health",
        columns: &[
            "id",
            "title",
            "scheduled_at",
            "kind",
            "location",
            "note",
            "completed_at",
            "created_at",
        ],
        blob_columns: &[],
    },
    TableSpec {
        name: "sexual_health_sti_tests",
        primary_key: "id",
        category: "sexual_health",
        columns: &["id", "tested_at", "test_type", "result", "note", "created_at"],
        blob_columns: &[],
    },
    TableSpec {
        name: "known_profiles",
        primary_key: "profile_id",
        category: "chat_messages",
        columns: &[
            "profile_id",
            "display_name",
            "primary_image_hash",
            "is_favorite",
            "updated_at",
        ],
        blob_columns: &[],
    },
    TableSpec {
        name: "profile_photo_history",
        primary_key: "id",
        category: "media_albums",
        columns: &["id", "profile_id", "image_hash", "first_seen_at", "last_seen_at"],
        blob_columns: &[],
    },
];

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportProgress<'a> {
    table: &'a str,
    table_index: usize,
    table_count: usize,
    rows_done: u64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImportProgress<'a> {
    table: &'a str,
    table_index: usize,
    table_count: usize,
    rows_done: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportOutcome {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    rows_imported: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupInspection {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    categories: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exported_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner_user_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_different_owner: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'static str>,
}

/// Falls back to this if an archive's manifest predates the `categories`
/// field (shouldn't happen in practice - export always writes it now - but
/// costs nothing to handle gracefully rather than hide every table).
const ALL_CATEGORY_IDS: &[&str] =
    &["chat_messages", "media_albums", "saved_phrases", "saved_locations", "settings", "sexual_health"];

enum ManifestCheck {
    Valid {
        categories: Vec<String>,
        exported_at: i64,
        manifest_owner: i64,
    },
    Invalid(&'static str),
}

/// Shared by `inspect_backup_file` and `import_backup_from_file` so the
/// version validation rules can't drift between the two. Cross-account
/// backups are accepted here so users can combine backups across accounts.
fn read_and_validate_manifest(
    archive: &mut ZipArchive<BufReader<File>>,
) -> Result<ManifestCheck, AppError> {
    let manifest: Value = {
        let mut manifest_file = archive
            .by_name("manifest.json")
            .map_err(|_| AppError::Backup("archive is missing manifest.json".into()))?;
        let mut text = String::new();
        manifest_file.read_to_string(&mut text)?;
        match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => return Ok(ManifestCheck::Invalid("invalid_format")),
        }
    };

    let version = manifest.get("version").and_then(Value::as_i64);
    if version != Some(BACKUP_VERSION) {
        return Ok(ManifestCheck::Invalid("invalid_format"));
    }
    let Some(manifest_owner) = manifest.get("ownerUserId").and_then(Value::as_i64) else {
        return Ok(ManifestCheck::Invalid("invalid_format"));
    };

    let exported_at = manifest.get("exportedAt").and_then(Value::as_i64).unwrap_or(0);
    let categories = manifest
        .get("categories")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_else(|| ALL_CATEGORY_IDS.iter().map(|s| s.to_string()).collect());

    Ok(ManifestCheck::Valid { categories, exported_at, manifest_owner })
}

fn resolve_db_path(app: &AppHandle, db_file_name: &str) -> Result<std::path::PathBuf, AppError> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| AppError::Backup(format!("failed to resolve app data dir: {e}")))?;
    Ok(dir.join(db_file_name))
}

fn sql_value_to_json(row: &rusqlite::Row, idx: usize) -> Result<Value, AppError> {
    use rusqlite::types::ValueRef;
    let value_ref = row.get_ref(idx)?;
    Ok(match value_ref {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(BASE64.encode(b)),
    })
}

fn json_to_rusqlite(value: &Value) -> Result<rusqlite::types::Value, AppError> {
    Ok(match value {
        Value::Null => rusqlite::types::Value::Null,
        Value::Bool(b) => rusqlite::types::Value::Integer(if *b { 1 } else { 0 }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                rusqlite::types::Value::Integer(i)
            } else if let Some(u) = n.as_u64() {
                rusqlite::types::Value::Integer(u as i64)
            } else if let Some(f) = n.as_f64() {
                rusqlite::types::Value::Real(f)
            } else {
                return Err(AppError::Backup("unsupported number value in row".into()));
            }
        }
        Value::String(s) => rusqlite::types::Value::Text(s.clone()),
        Value::Array(_) | Value::Object(_) => {
            return Err(AppError::Backup("unsupported nested value in row".into()));
        }
    })
}

fn build_upsert_sql(table: &TableSpec) -> String {
    let cols_csv = table.columns.join(", ");
    let placeholders = (1..=table.columns.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let t = table.name;
    let update_set = table
        .columns
        .iter()
        .copied()
        .filter(|&c| c != table.primary_key)
        .map(|c| match (t, c) {
            ("conversations", "other_profile_id" | "name") => {
                format!("{c} = COALESCE({t}.{c}, excluded.{c})")
            }
            ("conversations", "unread_count" | "pinned" | "muted" | "favorite" | "updated_at") => {
                format!("{c} = MAX({t}.{c}, excluded.{c})")
            }
            ("conversations", "created_at") => {
                "created_at = MIN(conversations.created_at, excluded.created_at)".to_owned()
            }
            ("conversations", "last_activity_timestamp" | "last_seen_in_inbox_at") => format!(
                "{c} = CASE WHEN {t}.{c} IS NULL THEN excluded.{c} WHEN excluded.{c} IS NULL THEN {t}.{c} ELSE MAX({t}.{c}, excluded.{c}) END"
            ),
            ("conversations", "preview_json") => {
                "preview_json = CASE WHEN COALESCE(excluded.last_activity_timestamp, 0) >= COALESCE(conversations.last_activity_timestamp, 0) THEN COALESCE(excluded.preview_json, conversations.preview_json) ELSE conversations.preview_json END".to_owned()
            }
            (
                "conversations",
                "participants_json" | "archived" | "archived_reason" | "archived_at" | "hidden",
            ) => format!(
                "{c} = CASE WHEN COALESCE(excluded.last_activity_timestamp, 0) >= COALESCE(conversations.last_activity_timestamp, 0) THEN excluded.{c} ELSE conversations.{c} END"
            ),
            ("conversation_meta", "last_read_timestamp") => {
                "last_read_timestamp = CASE WHEN conversation_meta.last_read_timestamp IS NULL THEN excluded.last_read_timestamp WHEN excluded.last_read_timestamp IS NULL THEN conversation_meta.last_read_timestamp ELSE MAX(conversation_meta.last_read_timestamp, excluded.last_read_timestamp) END".to_owned()
            }
            ("saved_phrases", "created_at") => {
                "created_at = MIN(saved_phrases.created_at, excluded.created_at)".to_owned()
            }
            ("media_files", "fetch_status") => {
                "fetch_status = CASE WHEN excluded.fetch_status = 'ready' OR media_files.fetch_status != 'ready' THEN excluded.fetch_status ELSE media_files.fetch_status END".to_owned()
            }
            ("known_profiles", "display_name" | "primary_image_hash") => {
                format!("{c} = COALESCE(NULLIF(excluded.{c}, ''), known_profiles.{c})")
            }
            ("known_profiles", "is_favorite" | "updated_at") => {
                format!("{c} = MAX(known_profiles.{c}, excluded.{c})")
            }
            ("profile_photo_history", "first_seen_at") => {
                "first_seen_at = MIN(profile_photo_history.first_seen_at, excluded.first_seen_at)".to_owned()
            }
            ("profile_photo_history", "last_seen_at") => {
                "last_seen_at = MAX(profile_photo_history.last_seen_at, excluded.last_seen_at)".to_owned()
            }
            (_, col) if table.blob_columns.contains(&col) => {
                format!("{col} = COALESCE(NULLIF(excluded.{col}, ''), {t}.{col})")
            }
            _ => format!("{c} = excluded.{c}"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {t} ({cols_csv}) VALUES ({placeholders}) ON CONFLICT({}) DO UPDATE SET {update_set}",
        table.primary_key
    )
}

fn remap_conversation_id(
    old_conv_id: &str,
    partner_to_conv_id: &HashMap<String, String>,
    explicit_other: Option<&str>,
    old_owner: i64,
    new_owner: i64,
) -> String {
    if old_owner == new_owner {
        return old_conv_id.to_owned();
    }
    if let Some(other) = explicit_other.filter(|s| !s.is_empty()) {
        if let Some(existing) = partner_to_conv_id.get(other) {
            return existing.clone();
        }
    }

    let old_owner_str = old_owner.to_string();
    let new_owner_str = new_owner.to_string();
    let mut parts = old_conv_id.split(':');
    if let (Some(p1), Some(p2), None) = (parts.next(), parts.next(), parts.next()) {
        let partner = if p1 == old_owner_str {
            Some(p2)
        } else if p2 == old_owner_str {
            Some(p1)
        } else {
            None
        };
        if let Some(partner_str) = partner {
            if let Some(existing) = partner_to_conv_id.get(partner_str) {
                return existing.clone();
            }
            if let (Ok(partner_num), Ok(owner_num)) =
                (partner_str.parse::<u64>(), new_owner_str.parse::<u64>())
            {
                let (lo, hi) = if owner_num <= partner_num {
                    (owner_num, partner_num)
                } else {
                    (partner_num, owner_num)
                };
                return format!("{lo}:{hi}");
            }
            return if p1 == old_owner_str {
                format!("{new_owner_str}:{partner_str}")
            } else {
                format!("{partner_str}:{new_owner_str}")
            };
        }
    }

    old_conv_id.to_owned()
}

fn remap_json_value(
    value: &mut Value,
    old_owner: i64,
    new_owner: i64,
    old_conv_id: Option<&str>,
    new_conv_id: Option<&str>,
) {
    match value {
        Value::Array(items) => {
            for item in items {
                remap_json_value(item, old_owner, new_owner, old_conv_id, new_conv_id);
            }
        }
        Value::Object(map) => {
            let old_owner_str = old_owner.to_string();
            let new_owner_str = new_owner.to_string();
            for (key, val) in map.iter_mut() {
                match key.as_str() {
                    "profileId" | "senderId" => {
                        if val.as_i64() == Some(old_owner) {
                            *val = json!(new_owner);
                        } else if val.as_str() == Some(old_owner_str.as_str()) {
                            *val = Value::String(new_owner_str.clone());
                        }
                    }
                    "conversationId" => {
                        if let (Some(old_c), Some(new_c)) = (old_conv_id, new_conv_id) {
                            if val.as_str() == Some(old_c) {
                                *val = Value::String(new_c.to_owned());
                            }
                        }
                    }
                    _ => remap_json_value(val, old_owner, new_owner, old_conv_id, new_conv_id),
                }
            }
        }
        _ => {}
    }
}

fn remap_json_string_field(
    row_map: &mut Map<String, Value>,
    field: &str,
    old_owner: i64,
    new_owner: i64,
    old_conv_id: Option<&str>,
    new_conv_id: Option<&str>,
) {
    let Some(Value::String(raw)) = row_map.get(field) else {
        return;
    };
    let Ok(mut parsed) = serde_json::from_str::<Value>(raw) else {
        return;
    };
    remap_json_value(&mut parsed, old_owner, new_owner, old_conv_id, new_conv_id);
    row_map.insert(field.to_string(), Value::String(parsed.to_string()));
}

fn remap_row_for_combine(
    table_name: &str,
    row_map: &mut Map<String, Value>,
    manifest_owner: i64,
    owner_user_id: i64,
    conv_id_map: &mut HashMap<String, String>,
    partner_to_conv_id: &HashMap<String, String>,
) {
    if manifest_owner == owner_user_id {
        return;
    }

    let old_conv = row_map
        .get("conversation_id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let resolve_conv = |old_c: &str, explicit_other: Option<&str>| -> String {
        conv_id_map.get(old_c).cloned().unwrap_or_else(|| {
            remap_conversation_id(
                old_c,
                partner_to_conv_id,
                explicit_other,
                manifest_owner,
                owner_user_id,
            )
        })
    };

    match table_name {
        "conversations" => {
            if let Some(ref old_c) = old_conv {
                let explicit_other = row_map
                    .get("other_profile_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let new_c = resolve_conv(old_c, explicit_other.as_deref());
                conv_id_map.insert(old_c.clone(), new_c.clone());
                row_map.insert("conversation_id".to_string(), Value::String(new_c.clone()));
                remap_json_string_field(
                    row_map,
                    "participants_json",
                    manifest_owner,
                    owner_user_id,
                    Some(old_c),
                    Some(&new_c),
                );
                remap_json_string_field(
                    row_map,
                    "preview_json",
                    manifest_owner,
                    owner_user_id,
                    Some(old_c),
                    Some(&new_c),
                );
            }
        }
        "conversation_meta" | "media_files" | "sexual_health_encounters" => {
            if let Some(ref old_c) = old_conv {
                let new_c = resolve_conv(old_c, None);
                row_map.insert("conversation_id".to_string(), Value::String(new_c));
            }
        }
        "messages" => {
            let new_c = old_conv.as_deref().map(|old_c| {
                let mapped = resolve_conv(old_c, None);
                row_map.insert("conversation_id".to_string(), Value::String(mapped.clone()));
                mapped
            });

            if let Some(sender_val) = row_map.get("sender_id") {
                if sender_val.as_i64() == Some(manifest_owner) {
                    row_map.insert("sender_id".to_string(), json!(owner_user_id));
                } else if sender_val.as_str() == Some(manifest_owner.to_string().as_str()) {
                    row_map.insert(
                        "sender_id".to_string(),
                        Value::String(owner_user_id.to_string()),
                    );
                }
            }

            if let (Some(old_c), Some(new_c_str)) = (old_conv.as_deref(), new_c.as_deref()) {
                if new_c_str != old_c {
                    if let Some(Value::String(msg_id)) = row_map.get("message_id") {
                        if msg_id.starts_with("local-system:") && msg_id.contains(old_c) {
                            let updated_id = msg_id.replace(old_c, new_c_str);
                            row_map.insert("message_id".to_string(), Value::String(updated_id));
                        }
                    }
                }
            }

            remap_json_string_field(
                row_map,
                "body_json",
                manifest_owner,
                owner_user_id,
                old_conv.as_deref(),
                new_c.as_deref(),
            );
            remap_json_string_field(
                row_map,
                "reply_preview_json",
                manifest_owner,
                owner_user_id,
                old_conv.as_deref(),
                new_c.as_deref(),
            );
        }
        "albums" => {
            let old_owner_str = manifest_owner.to_string();
            let new_owner_str = owner_user_id.to_string();
            if let Some(owner_val) = row_map.get("owner_profile_id") {
                if owner_val.as_str() == Some(old_owner_str.as_str())
                    || owner_val.as_i64() == Some(manifest_owner)
                {
                    row_map.insert(
                        "owner_profile_id".to_string(),
                        Value::String(new_owner_str),
                    );
                }
            }
            if let Some(ref old_c) = old_conv {
                let new_c = resolve_conv(old_c, None);
                row_map.insert("conversation_id".to_string(), Value::String(new_c));
            }
        }
        _ => {}
    }
}

fn load_existing_partner_map(conn: &Connection) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT other_profile_id, conversation_id FROM conversations WHERE other_profile_id IS NOT NULL",
    ) else {
        return map;
    };
    let Ok(mut rows) = stmt.query([]) else {
        return map;
    };
    while let Ok(Some(row)) = rows.next() {
        if let (Ok(other_id), Ok(conv_id)) = (row.get::<_, String>(0), row.get::<_, String>(1)) {
            if !other_id.is_empty() && !conv_id.is_empty() {
                map.insert(other_id, conv_id);
            }
        }
    }
    map
}

fn table_exists(conn: &Connection, table_name: &str) -> Result<bool, AppError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table_name],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

fn ensure_optional_directory_tables(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS known_profiles (
            profile_id TEXT PRIMARY KEY,
            display_name TEXT,
            primary_image_hash TEXT,
            is_favorite INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS profile_photo_history (
            id TEXT PRIMARY KEY,
            profile_id TEXT NOT NULL,
            image_hash TEXT NOT NULL,
            first_seen_at INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL
        );
        "#,
    )?;
    Ok(())
}

fn export_backup_to_path<F>(
    db_path: &Path,
    owner_user_id: i64,
    dest_path: &Path,
    categories: &[String],
    mut on_progress: F,
) -> Result<(), AppError>
where
    F: FnMut(ExportProgress<'_>),
{
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

    let selected_tables: Vec<&TableSpec> = EXPORT_TABLES
        .iter()
        .filter(|t| categories.iter().any(|c| c == t.category))
        .collect();

    let file = File::create(dest_path)?;
    let mut zip = ZipWriter::new(BufWriter::new(file));
    let text_options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let blob_options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    zip.start_file("manifest.json", text_options)?;
    let manifest = json!({
        "version": BACKUP_VERSION,
        "exportedAt": chrono::Utc::now().timestamp_millis(),
        "ownerUserId": owner_user_id,
        "categories": categories,
    });
    zip.write_all(manifest.to_string().as_bytes())?;

    let mut media_counter: u64 = 0;
    let table_count = selected_tables.len();

    for (table_index, table) in selected_tables.into_iter().enumerate() {
        let entry_name = format!("tables/{}.jsonl", table.name);
        zip.start_file(&entry_name, text_options)?;

        if !table_exists(&conn, table.name)? {
            on_progress(ExportProgress { table: table.name, table_index, table_count, rows_done: 0 });
            continue;
        }

        let select_sql = format!(
            "SELECT {} FROM {} ORDER BY {}",
            table.columns.join(", "),
            table.name,
            table.primary_key
        );
        let mut stmt = conn
            .prepare(&select_sql)
            .map_err(|e| AppError::Backup(format!("failed to query {}: {e}", table.name)))?;
        let mut rows = stmt
            .query([])
            .map_err(|e| AppError::Backup(format!("failed to query {}: {e}", table.name)))?;

        let mut rows_done: u64 = 0;
        while let Some(row) = rows
            .next()
            .map_err(|e| AppError::Backup(format!("failed to read row from {}: {e}", table.name)))?
        {
            let mut obj = Map::new();
            for (col_index, col) in table.columns.iter().copied().enumerate() {
                let is_blob = table.blob_columns.contains(&col);
                if is_blob {
                    let value: Option<String> = row
                        .get(col_index)
                        .map_err(|e| AppError::Backup(format!("failed to read {}.{col}: {e}", table.name)))?;
                    match value {
                        Some(b64_text) => {
                            let bytes = BASE64.decode(b64_text.as_bytes()).map_err(|e| {
                                AppError::Backup(format!("invalid base64 in {}.{col}: {e}", table.name))
                            })?;
                            let media_path = format!("media/{media_counter}.bin");
                            media_counter += 1;
                            zip.start_file(&media_path, blob_options)?;
                            zip.write_all(&bytes)?;
                            obj.insert(col.to_string(), json!({ "$mediaRef": media_path }));
                        }
                        None => {
                            obj.insert(col.to_string(), Value::Null);
                        }
                    }
                } else {
                    obj.insert(col.to_string(), sql_value_to_json(row, col_index)?);
                }
            }

            zip.write_all(Value::Object(obj).to_string().as_bytes())?;
            zip.write_all(b"\n")?;

            rows_done += 1;
            if rows_done % PROGRESS_EVERY_N_ROWS == 0 {
                on_progress(ExportProgress { table: table.name, table_index, table_count, rows_done });
            }
        }

        on_progress(ExportProgress { table: table.name, table_index, table_count, rows_done });
    }

    zip.finish()?;
    Ok(())
}

#[tauri::command]
pub fn export_backup_to_file(
    app: AppHandle,
    db_file_name: String,
    owner_user_id: i64,
    dest_path: String,
    categories: Vec<String>,
) -> Result<(), AppError> {
    let db_path = resolve_db_path(&app, &db_file_name)?;
    export_backup_to_path(&db_path, owner_user_id, Path::new(&dest_path), &categories, |progress| {
        let _ = app.emit(EXPORT_PROGRESS_EVENT, progress);
    })
}

fn inspect_backup_at_path(src_path: &Path, owner_user_id: i64) -> Result<BackupInspection, AppError> {
    let file = File::open(src_path)?;
    let mut archive = ZipArchive::new(BufReader::new(file))?;

    Ok(match read_and_validate_manifest(&mut archive)? {
        ManifestCheck::Invalid(error) => BackupInspection {
            ok: false,
            categories: None,
            exported_at: None,
            owner_user_id: None,
            is_different_owner: None,
            error: Some(error),
        },
        ManifestCheck::Valid { categories, exported_at, manifest_owner } => BackupInspection {
            ok: true,
            categories: Some(categories),
            exported_at: Some(exported_at),
            owner_user_id: Some(manifest_owner),
            is_different_owner: Some(manifest_owner != owner_user_id),
            error: None,
        },
    })
}

#[tauri::command]
pub fn inspect_backup_file(src_path: String, owner_user_id: i64) -> Result<BackupInspection, AppError> {
    inspect_backup_at_path(Path::new(&src_path), owner_user_id)
}

fn import_backup_from_path<F>(
    db_path: &Path,
    owner_user_id: i64,
    src_path: &Path,
    categories: &[String],
    mut on_progress: F,
) -> Result<ImportOutcome, AppError>
where
    F: FnMut(ImportProgress<'_>),
{
    let file = File::open(src_path)?;
    let mut archive = ZipArchive::new(BufReader::new(file))?;

    let manifest_owner = match read_and_validate_manifest(&mut archive)? {
        ManifestCheck::Invalid(error) => {
            return Ok(ImportOutcome { ok: false, rows_imported: None, error: Some(error) });
        }
        ManifestCheck::Valid { manifest_owner, .. } => manifest_owner,
    };

    let selected_tables: Vec<&TableSpec> = EXPORT_TABLES
        .iter()
        .filter(|t| categories.iter().any(|c| c == t.category))
        .collect();

    let mut conn = Connection::open(db_path)?;
    conn.execute_batch("PRAGMA busy_timeout = 5000;")?;
    ensure_optional_directory_tables(&conn)?;
    let partner_to_conv_id = if manifest_owner != owner_user_id {
        load_existing_partner_map(&conn)
    } else {
        HashMap::new()
    };
    let mut conv_id_map: HashMap<String, String> = HashMap::new();

    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    let mut total_rows_imported: u64 = 0;
    let table_count = selected_tables.len();

    for (table_index, table) in selected_tables.into_iter().enumerate() {
        let entry_name = format!("tables/{}.jsonl", table.name);
        let lines: Vec<String> = match archive.by_name(&entry_name) {
            Ok(entry) => BufReader::new(entry).lines().collect::<std::io::Result<Vec<_>>>()?,
            Err(ZipError::FileNotFound) => Vec::new(),
            Err(e) => return Err(e.into()),
        };

        let upsert_sql = build_upsert_sql(table);
        let mut rows_done: u64 = 0;

        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let row_value: Value = serde_json::from_str(&line)
                .map_err(|e| AppError::Backup(format!("corrupt row in {}: {e}", table.name)))?;
            let Value::Object(mut row_map) = row_value else {
                continue;
            };

            remap_row_for_combine(
                table.name,
                &mut row_map,
                manifest_owner,
                owner_user_id,
                &mut conv_id_map,
                &partner_to_conv_id,
            );

            let pk_present = row_map.get(table.primary_key).map(|v| !v.is_null()).unwrap_or(false);
            if !pk_present {
                continue;
            }

            for blob_col in table.blob_columns.iter().copied() {
                let resolved = match row_map.get(blob_col) {
                    Some(Value::Object(ref_obj)) => {
                        let media_path = ref_obj.get("$mediaRef").and_then(Value::as_str).ok_or_else(|| {
                            AppError::Backup(format!("missing $mediaRef in {}.{blob_col}", table.name))
                        })?;
                        let mut media_file = archive.by_name(media_path)?;
                        let mut bytes = Vec::new();
                        media_file.read_to_end(&mut bytes)?;
                        Value::String(BASE64.encode(bytes))
                    }
                    Some(Value::Null) | None => Value::Null,
                    Some(_) => {
                        return Err(AppError::Backup(format!(
                            "unexpected value for blob column {}.{blob_col}",
                            table.name
                        )));
                    }
                };
                row_map.insert(blob_col.to_string(), resolved);
            }

            let params = table
                .columns
                .iter()
                .copied()
                .map(|c| json_to_rusqlite(row_map.get(c).unwrap_or(&Value::Null)))
                .collect::<Result<Vec<_>, AppError>>()?;

            tx.execute(&upsert_sql, rusqlite::params_from_iter(params.iter()))?;

            rows_done += 1;
            total_rows_imported += 1;
            if rows_done % PROGRESS_EVERY_N_ROWS == 0 {
                on_progress(ImportProgress { table: table.name, table_index, table_count, rows_done });
            }
        }

        on_progress(ImportProgress { table: table.name, table_index, table_count, rows_done });
    }

    tx.commit()?;

    Ok(ImportOutcome { ok: true, rows_imported: Some(total_rows_imported), error: None })
}

#[tauri::command]
pub fn import_backup_from_file(
    app: AppHandle,
    db_file_name: String,
    owner_user_id: i64,
    src_path: String,
    categories: Vec<String>,
) -> Result<ImportOutcome, AppError> {
    let db_path = resolve_db_path(&app, &db_file_name)?;
    import_backup_from_path(&db_path, owner_user_id, Path::new(&src_path), &categories, |progress| {
        let _ = app.emit(IMPORT_PROGRESS_EVENT, progress);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let unique = format!(
                "fg-backup-test-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            );
            let path = std::env::temp_dir().join(unique);
            std::fs::create_dir_all(&path).expect("create test temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn init_backup_test_schema(conn: &Connection) {
        conn.execute_batch(
            r#"
            CREATE TABLE conversations (
                conversation_id TEXT PRIMARY KEY,
                other_profile_id TEXT,
                name TEXT,
                participants_json TEXT NOT NULL,
                last_activity_timestamp INTEGER,
                unread_count INTEGER NOT NULL DEFAULT 0,
                pinned INTEGER NOT NULL DEFAULT 0,
                muted INTEGER NOT NULL DEFAULT 0,
                favorite INTEGER NOT NULL DEFAULT 0,
                preview_json TEXT,
                archived INTEGER NOT NULL DEFAULT 0,
                archived_reason TEXT,
                archived_at INTEGER,
                hidden INTEGER NOT NULL DEFAULT 0,
                last_seen_in_inbox_at INTEGER,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE conversation_meta (
                conversation_id TEXT PRIMARY KEY,
                last_read_timestamp INTEGER
            );
            CREATE TABLE messages (
                message_id TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL,
                sender_id INTEGER NOT NULL,
                timestamp INTEGER NOT NULL,
                type TEXT,
                chat1_type TEXT,
                body_json TEXT,
                unsent INTEGER NOT NULL DEFAULT 0,
                local_history INTEGER NOT NULL DEFAULT 0,
                reply_to_message_id TEXT,
                reply_preview_json TEXT,
                reactions_json TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE media_files (
                media_key TEXT PRIMARY KEY,
                conversation_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                mime_type TEXT,
                data_base64 TEXT NOT NULL,
                view_once INTEGER NOT NULL DEFAULT 0,
                size_bytes INTEGER NOT NULL DEFAULT 0,
                fetch_status TEXT NOT NULL DEFAULT 'ready',
                fetched_at INTEGER
            );
            CREATE TABLE albums (
                album_id TEXT PRIMARY KEY,
                owner_profile_id TEXT,
                album_name TEXT,
                conversation_id TEXT,
                shared_via_message_id TEXT,
                preview_cover_base64 TEXT,
                preview_cover_mime_type TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE album_media (
                content_id TEXT PRIMARY KEY,
                album_id TEXT NOT NULL,
                content_type TEXT NOT NULL,
                data_base64 TEXT,
                thumb_data_base64 TEXT,
                remaining_views INTEGER,
                is_viewable INTEGER NOT NULL DEFAULT 1,
                fetched_at INTEGER NOT NULL
            );
            CREATE TABLE avatars (
                media_hash TEXT PRIMARY KEY,
                data_base64 TEXT NOT NULL,
                mime_type TEXT,
                fetched_at INTEGER NOT NULL
            );
            CREATE TABLE settings (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE saved_phrases (
                phrase TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE saved_locations (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                geohash TEXT NOT NULL,
                lat REAL NOT NULL,
                lon REAL NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE sexual_health_prep_doses (
                id TEXT PRIMARY KEY,
                taken_at INTEGER NOT NULL,
                scheme TEXT NOT NULL,
                dose_role TEXT,
                note TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE sexual_health_encounters (
                id TEXT PRIMARY KEY,
                occurred_at INTEGER NOT NULL,
                profile_id TEXT,
                display_name TEXT,
                tags_json TEXT NOT NULL,
                note TEXT,
                conversation_id TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE sexual_health_appointments (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                scheduled_at INTEGER NOT NULL,
                kind TEXT NOT NULL,
                location TEXT,
                note TEXT,
                completed_at INTEGER,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE sexual_health_sti_tests (
                id TEXT PRIMARY KEY,
                tested_at INTEGER NOT NULL,
                test_type TEXT NOT NULL,
                result TEXT NOT NULL,
                note TEXT,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE known_profiles (
                profile_id TEXT PRIMARY KEY,
                display_name TEXT,
                primary_image_hash TEXT,
                is_favorite INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE profile_photo_history (
                id TEXT PRIMARY KEY,
                profile_id TEXT NOT NULL,
                image_hash TEXT NOT NULL,
                first_seen_at INTEGER NOT NULL,
                last_seen_at INTEGER NOT NULL
            );
            "#,
        )
        .expect("init test schema");
    }

    #[test]
    fn selective_restore_imports_only_chosen_categories() {
        let dir = TestDir::new("selective");
        let src_db = dir.path().join("src.db");
        let dst_db = dir.path().join("dst.db");
        let zip_path = dir.path().join("backup.zip");

        let src = Connection::open(&src_db).unwrap();
        init_backup_test_schema(&src);
        src.execute(
            "INSERT INTO saved_phrases (phrase, created_at) VALUES ('Hey there', 100)",
            [],
        )
        .unwrap();
        src.execute(
            "INSERT INTO saved_locations (id, name, geohash, lat, lon, created_at) VALUES ('loc1', 'Home', 'u4pruydqqvj', 1.0, 2.0, 100)",
            [],
        )
        .unwrap();
        src.execute(
            "INSERT INTO settings (key, value) VALUES ('theme', '\"dark\"')",
            [],
        )
        .unwrap();

        let all: Vec<String> = ALL_CATEGORY_IDS.iter().map(|s| (*s).to_owned()).collect();
        export_backup_to_path(&src_db, 100, &zip_path, &all, |_| {}).unwrap();

        let dst = Connection::open(&dst_db).unwrap();
        init_backup_test_schema(&dst);
        dst.execute(
            "INSERT INTO settings (key, value) VALUES ('theme', '\"light\"')",
            [],
        )
        .unwrap();

        let outcome = import_backup_from_path(
            &dst_db,
            100,
            &zip_path,
            &["saved_phrases".to_owned()],
            |_| {},
        )
        .unwrap();
        assert!(outcome.ok);
        assert_eq!(outcome.rows_imported, Some(1));

        let phrase_count: i64 = dst
            .query_row("SELECT COUNT(*) FROM saved_phrases", [], |r| r.get(0))
            .unwrap();
        let loc_count: i64 = dst
            .query_row("SELECT COUNT(*) FROM saved_locations", [], |r| r.get(0))
            .unwrap();
        let theme: String = dst
            .query_row("SELECT value FROM settings WHERE key = 'theme'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(phrase_count, 1);
        assert_eq!(loc_count, 0);
        assert_eq!(theme, "\"light\"");
    }

    #[test]
    fn combine_same_account_preserves_newer_metadata_and_merges_rows() {
        let dir = TestDir::new("same-combine");
        let src_db = dir.path().join("src.db");
        let dst_db = dir.path().join("dst.db");
        let zip_path = dir.path().join("older.zip");

        let src = Connection::open(&src_db).unwrap();
        init_backup_test_schema(&src);
        src.execute(
            "INSERT INTO conversations (conversation_id, other_profile_id, name, participants_json, last_activity_timestamp, unread_count, pinned, muted, favorite, preview_json, created_at, updated_at) VALUES ('100:200', '200', 'Alex', '[]', 1000, 0, 0, 0, 1, '{\"text\":\"old\"}', 500, 1000)",
            [],
        )
        .unwrap();
        src.execute(
            "INSERT INTO conversations (conversation_id, other_profile_id, name, participants_json, last_activity_timestamp, unread_count, pinned, muted, favorite, preview_json, created_at, updated_at) VALUES ('100:300', '300', 'Sam', '[]', 900, 1, 0, 0, 0, '{\"text\":\"hi sam\"}', 600, 900)",
            [],
        )
        .unwrap();
        src.execute(
            "INSERT INTO messages (message_id, conversation_id, sender_id, timestamp, created_at, updated_at) VALUES ('m-old', '100:200', 200, 1000, 1000, 1000)",
            [],
        )
        .unwrap();
        src.execute(
            "INSERT INTO saved_phrases (phrase, created_at) VALUES ('Phrase A', 100), ('Phrase B', 200)",
            [],
        )
        .unwrap();

        export_backup_to_path(
            &src_db,
            100,
            &zip_path,
            &["chat_messages".to_owned(), "saved_phrases".to_owned()],
            |_| {},
        )
        .unwrap();

        let dst = Connection::open(&dst_db).unwrap();
        init_backup_test_schema(&dst);
        dst.execute(
            "INSERT INTO conversations (conversation_id, other_profile_id, name, participants_json, last_activity_timestamp, unread_count, pinned, muted, favorite, preview_json, created_at, updated_at) VALUES ('100:200', '200', 'Alex', '[]', 2000, 2, 1, 0, 0, '{\"text\":\"newest\"}', 800, 2000)",
            [],
        )
        .unwrap();
        dst.execute(
            "INSERT INTO messages (message_id, conversation_id, sender_id, timestamp, created_at, updated_at) VALUES ('m-new', '100:200', 100, 2000, 2000, 2000)",
            [],
        )
        .unwrap();
        dst.execute(
            "INSERT INTO saved_phrases (phrase, created_at) VALUES ('Phrase C', 300)",
            [],
        )
        .unwrap();

        let outcome = import_backup_from_path(
            &dst_db,
            100,
            &zip_path,
            &["chat_messages".to_owned(), "saved_phrases".to_owned()],
            |_| {},
        )
        .unwrap();
        assert!(outcome.ok);

        let (last_ts, pinned, favorite, preview, created_at): (i64, i64, i64, String, i64) = dst
            .query_row(
                "SELECT last_activity_timestamp, pinned, favorite, preview_json, created_at FROM conversations WHERE conversation_id = '100:200'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(last_ts, 2000);
        assert_eq!(pinned, 1);
        assert_eq!(favorite, 1);
        assert_eq!(preview, "{\"text\":\"newest\"}");
        assert_eq!(created_at, 500);

        let conv_count: i64 = dst
            .query_row("SELECT COUNT(*) FROM conversations", [], |r| r.get(0))
            .unwrap();
        let msg_count: i64 = dst
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        let phrase_count: i64 = dst
            .query_row("SELECT COUNT(*) FROM saved_phrases", [], |r| r.get(0))
            .unwrap();
        assert_eq!(conv_count, 2);
        assert_eq!(msg_count, 2);
        assert_eq!(phrase_count, 3);
    }

    #[test]
    fn combine_cross_account_remaps_owner_and_conversations() {
        let dir = TestDir::new("cross-combine");
        let src_db = dir.path().join("src100.db");
        let dst_db = dir.path().join("dst200.db");
        let zip_path = dir.path().join("acct100.zip");

        let src = Connection::open(&src_db).unwrap();
        init_backup_test_schema(&src);
        src.execute(
            "INSERT INTO conversations (conversation_id, other_profile_id, name, participants_json, last_activity_timestamp, preview_json, created_at, updated_at) VALUES ('100:300', '300', 'Sam', '[{\"profileId\":100},{\"profileId\":300}]', 1500, '{\"conversationId\":\"100:300\",\"senderId\":100}', 500, 1500)",
            [],
        )
        .unwrap();
        src.execute(
            "INSERT INTO conversations (conversation_id, other_profile_id, name, participants_json, last_activity_timestamp, preview_json, created_at, updated_at) VALUES ('100:400', '400', 'Pat', '[{\"profileId\":100},{\"profileId\":400}]', 1600, NULL, 600, 1600)",
            [],
        )
        .unwrap();
        src.execute(
            "INSERT INTO messages (message_id, conversation_id, sender_id, timestamp, created_at, updated_at) VALUES ('m-100-out', '100:300', 100, 1400, 1400, 1400), ('m-300-in', '100:300', 300, 1500, 1500, 1500)",
            [],
        )
        .unwrap();
        src.execute(
            "INSERT INTO albums (album_id, owner_profile_id, album_name, conversation_id, created_at, updated_at) VALUES ('alb-1', '100', 'My Pix', '100:300', 100, 100)",
            [],
        )
        .unwrap();

        export_backup_to_path(
            &src_db,
            100,
            &zip_path,
            &["chat_messages".to_owned(), "media_albums".to_owned()],
            |_| {},
        )
        .unwrap();

        let inspection = inspect_backup_at_path(&zip_path, 200).unwrap();
        assert!(inspection.ok);
        assert_eq!(inspection.owner_user_id, Some(100));
        assert_eq!(inspection.is_different_owner, Some(true));

        let dst = Connection::open(&dst_db).unwrap();
        init_backup_test_schema(&dst);
        dst.execute(
            "INSERT INTO conversations (conversation_id, other_profile_id, name, participants_json, last_activity_timestamp, created_at, updated_at) VALUES ('200:300', '300', 'Sam', '[{\"profileId\":200},{\"profileId\":300}]', 1200, 400, 1200)",
            [],
        )
        .unwrap();
        dst.execute(
            "INSERT INTO messages (message_id, conversation_id, sender_id, timestamp, created_at, updated_at) VALUES ('m-existing', '200:300', 200, 1200, 1200, 1200)",
            [],
        )
        .unwrap();

        let outcome = import_backup_from_path(
            &dst_db,
            200,
            &zip_path,
            &["chat_messages".to_owned(), "media_albums".to_owned()],
            |_| {},
        )
        .unwrap();
        assert!(outcome.ok);

        let sam_msgs: i64 = dst
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE conversation_id = '200:300'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let remapped_sender: i64 = dst
            .query_row(
                "SELECT sender_id FROM messages WHERE message_id = 'm-100-out'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let kept_partner_sender: i64 = dst
            .query_row(
                "SELECT sender_id FROM messages WHERE message_id = 'm-300-in'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let pat_conv_exists: i64 = dst
            .query_row(
                "SELECT COUNT(*) FROM conversations WHERE conversation_id = '200:400'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let (album_owner, album_conv): (String, String) = dst
            .query_row(
                "SELECT owner_profile_id, conversation_id FROM albums WHERE album_id = 'alb-1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();

        assert_eq!(sam_msgs, 3);
        assert_eq!(remapped_sender, 200);
        assert_eq!(kept_partner_sender, 300);
        assert_eq!(pat_conv_exists, 1);
        assert_eq!(album_owner, "200");
        assert_eq!(album_conv, "200:300");
    }

    #[test]
    fn combine_merges_known_profiles_and_photo_history() {
        let dir = TestDir::new("known-profiles");
        let src_db = dir.path().join("src.db");
        let dst_db = dir.path().join("dst.db");
        let zip_path = dir.path().join("profiles.zip");

        let src = Connection::open(&src_db).unwrap();
        init_backup_test_schema(&src);
        src.execute(
            "INSERT INTO known_profiles (profile_id, display_name, primary_image_hash, is_favorite, updated_at) VALUES ('555', 'Jordan', 'hash_new', 1, 2000)",
            [],
        )
        .unwrap();
        src.execute(
            "INSERT INTO profile_photo_history (id, profile_id, image_hash, first_seen_at, last_seen_at) VALUES ('555:hash_old', '555', 'hash_old', 500, 1000), ('555:hash_new', '555', 'hash_new', 2000, 2000)",
            [],
        )
        .unwrap();

        export_backup_to_path(
            &src_db,
            100,
            &zip_path,
            &["chat_messages".to_owned(), "media_albums".to_owned()],
            |_| {},
        )
        .unwrap();

        let dst = Connection::open(&dst_db).unwrap();
        init_backup_test_schema(&dst);
        dst.execute(
            "INSERT INTO known_profiles (profile_id, display_name, primary_image_hash, is_favorite, updated_at) VALUES ('555', NULL, 'hash_old', 0, 1000)",
            [],
        )
        .unwrap();
        dst.execute(
            "INSERT INTO profile_photo_history (id, profile_id, image_hash, first_seen_at, last_seen_at) VALUES ('555:hash_old', '555', 'hash_old', 800, 1200)",
            [],
        )
        .unwrap();

        let outcome = import_backup_from_path(
            &dst_db,
            200,
            &zip_path,
            &["chat_messages".to_owned(), "media_albums".to_owned()],
            |_| {},
        )
        .unwrap();
        assert!(outcome.ok);

        let (name, hash, is_fav): (Option<String>, Option<String>, i64) = dst
            .query_row(
                "SELECT display_name, primary_image_hash, is_favorite FROM known_profiles WHERE profile_id = '555'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let (first_seen, last_seen): (i64, i64) = dst
            .query_row(
                "SELECT first_seen_at, last_seen_at FROM profile_photo_history WHERE id = '555:hash_old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let history_count: i64 = dst
            .query_row(
                "SELECT COUNT(*) FROM profile_photo_history WHERE profile_id = '555'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        assert_eq!(name.as_deref(), Some("Jordan"));
        assert_eq!(hash.as_deref(), Some("hash_new"));
        assert_eq!(is_fav, 1);
        assert_eq!(first_seen, 500);
        assert_eq!(last_seen, 1200);
        assert_eq!(history_count, 2);
    }
}
