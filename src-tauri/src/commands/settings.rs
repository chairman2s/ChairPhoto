//! Settings and external-module discovery: the catalog key/value settings surface,
//! the compiled-in plugin feature list, and the on-disk external module manifests.
//!
//! See `docs/plugin-system.md` (H8) for the manifest format and requirement checks.

use super::*;
use serde::{Deserialize, Serialize};
use tauri::State;

/// Which plugin backend features were compiled into this build (e.g. "ai"). The
/// frontend uses this so a module whose backend was compiled out stays inert.
#[tauri::command]
pub fn plugin_features() -> Vec<String> {
    // Every push below is feature-gated, so a build with none of them never mutates this.
    #[allow(unused_mut)]
    let mut features = Vec::new();
    #[cfg(feature = "ai")]
    features.push("ai".to_string());
    #[cfg(feature = "edit")]
    features.push("edit".to_string());
    #[cfg(feature = "instagram")]
    features.push("instagram".to_string());
    #[cfg(feature = "flickr")]
    features.push("flickr".to_string());
    #[cfg(feature = "smugmug")]
    features.push("smugmug".to_string());
    #[cfg(feature = "localsend")]
    features.push("localsend".to_string());
    #[cfg(feature = "collage")]
    features.push("collage".to_string());
    #[cfg(feature = "slideshow")]
    features.push("slideshow".to_string());
    #[cfg(feature = "map")]
    features.push("map".to_string());
    #[cfg(feature = "faces")]
    features.push("faces".to_string());
    #[cfg(feature = "smarttags")]
    features.push("smarttags".to_string());
    // Not a plugin backend but reported the same way (#49): a module that cannot work without
    // the host-mediated network proxy declares `backendFeature: "module-fetch"` and is blocked
    // with "backend not included in this build" on a build that compiled it out, instead of
    // enabling and then failing at its first request.
    #[cfg(feature = "module-fetch")]
    features.push("module-fetch".to_string());
    features
}

/// Read a settings value (used for module-scoped, namespaced settings).
#[tauri::command]
pub fn get_setting(state: State<'_, AppState>, key: String) -> Result<Option<String>, String> {
    with_catalog(&state, |c| c.get_setting(&key))
}

/// Write a settings value (used for module-scoped, namespaced settings).
#[tauri::command]
pub fn set_setting(state: State<'_, AppState>, key: String, value: String) -> Result<(), String> {
    with_catalog(&state, |c| c.set_setting(&key, &value))
}

// ── External module discovery (H8b) ──────────────────────────────────────────

/// A deserialized `chairphoto-module.json` manifest from the user's modules directory.
///
/// Fields mirror the manifest spec in `docs/plugin-system.md`. Unknown fields in the
/// JSON are silently ignored (forward-compatible). Optional fields use `Option<T>` so a
/// missing key never causes a deserialization failure.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalModuleManifest {
    /// Stable unique identifier — must match the `ChairPhotoModule.id` in the entrypoint.
    pub id: String,
    /// Human-readable display name shown in the Modules panel.
    pub name: String,
    /// Module version in `MAJOR.MINOR.PATCH` format (used by the `requires`/`satisfies` logic).
    pub version: String,
    /// One-line description shown in the Modules panel.
    pub description: String,
    /// Path to the JS entry module relative to the module directory. Its default export
    /// must be a valid `ChairPhotoModule`.
    pub entrypoint: String,
    /// The Cargo feature the module's backend needs (e.g. `"ai"`). Absent if the module is
    /// frontend-only. The host shows "backend not included" when this feature is absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_feature: Option<String>,
    /// Inter-module dependencies. Same shape as `ChairPhotoModule.requires`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires: Option<Vec<ModuleRequirement>>,
    /// Lowest host (app) version this module supports. The host refuses to enable the
    /// module when its own version is older than this.
    pub min_host_version: String,
    /// The backend surface this module declares it needs. Absent means "nothing": the host
    /// then refuses every `api.invoke` from it.
    ///
    /// This is read here, from the manifest, *without executing the module* — which is what
    /// makes it reviewable before any of its code runs, and why the manifest (not the
    /// imported module object) is authoritative for an external module's permissions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<ModulePermissions>,
    /// Absolute path to the module directory on disk. Injected by `list_external_modules`
    /// so the frontend can construct the full entrypoint path via `convertFileSrc`.
    /// Never present in the JSON file on disk — populated at runtime, so skip deserialization.
    #[serde(skip_deserializing, default)]
    pub module_dir: String,
}

/// The permissions a module declares in its manifest.
///
/// Deliberately a struct rather than a flat string list (#48), which is what let step 4
/// (#49) add `origins` beside `commands` without breaking a manifest already on disk: a
/// pre-#49 manifest simply declares no origins, and `api.fetch` refuses everything for it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModulePermissions {
    /// Exact backend command names the module may pass to `api.invoke`. Matching in the
    /// host is exact string equality — there are no wildcards, so `"*"` is not a grant of
    /// everything, it is a permission to invoke a command literally named `*`.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Origins the module may reach through `api.fetch` — `https://host` or
    /// `https://host:port`, matched exactly after normalisation, with no wildcards for the
    /// same reason `commands` has none: `*.example.com` would cover hosts that do not exist
    /// yet and can be created by whoever controls the domain.
    ///
    /// The host (`src/modules/host.ts`) is what enforces this list, because it is the only
    /// layer that knows which module is calling; `commands/net.rs` independently enforces the
    /// invariants that hold for any request (https, no redirects, public addresses only).
    #[serde(default)]
    pub origins: Vec<String>,
}

/// A single inter-module dependency declared in the manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModuleRequirement {
    /// The id of the required module.
    pub id: String,
    /// Optional semver range. Absent means any version is accepted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// Inner blocking implementation for [`list_external_modules`]. Separated so tests can call
/// it directly without a Tauri runtime, and so the public command can wrap it in
/// `spawn_blocking` to stay off the async executor while doing real filesystem I/O.
fn list_external_modules_blocking() -> Result<Vec<ExternalModuleManifest>, String> {
    let modules_dir = match app_data_dir() {
        Ok(d) => d.join("modules"),
        Err(e) => {
            eprintln!("[H8b] could not determine app_data_dir: {e}");
            return Ok(Vec::new());
        }
    };

    // An absent modules directory is normal (no modules installed yet).
    if !modules_dir.exists() {
        return Ok(Vec::new());
    }

    let entries = match std::fs::read_dir(&modules_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!(
                "[H8b] could not read modules dir {}: {e}",
                modules_dir.display()
            );
            return Ok(Vec::new());
        }
    };

    let mut manifests = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[H8b] error reading modules dir entry: {e}");
                continue;
            }
        };

        let module_dir = entry.path();
        if !module_dir.is_dir() {
            continue;
        }

        let manifest_path = module_dir.join("chairphoto-module.json");
        if !manifest_path.exists() {
            eprintln!(
                "[H8b] skipping {}: chairphoto-module.json not found",
                module_dir.display()
            );
            continue;
        }

        let raw = match std::fs::read_to_string(&manifest_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "[H8b] skipping {}: could not read chairphoto-module.json: {e}",
                    module_dir.display()
                );
                continue;
            }
        };

        // Deserialize into a partial struct first so we can inject the module_dir before
        // constructing the final ExternalModuleManifest.
        let partial: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "[H8b] skipping {}: malformed JSON in chairphoto-module.json: {e}",
                    manifest_path.display()
                );
                continue;
            }
        };

        // Helper closure: extract a required string field.
        let req_str = |key: &str| -> Option<String> {
            partial.get(key).and_then(|v| v.as_str()).map(|s| s.to_owned())
        };

        let id = match req_str("id") {
            Some(v) => v,
            None => {
                eprintln!(
                    "[H8b] skipping {}: missing required field `id`",
                    manifest_path.display()
                );
                continue;
            }
        };
        let name = match req_str("name") {
            Some(v) => v,
            None => {
                eprintln!(
                    "[H8b] skipping {}: missing required field `name`",
                    manifest_path.display()
                );
                continue;
            }
        };
        let version = match req_str("version") {
            Some(v) => v,
            None => {
                eprintln!(
                    "[H8b] skipping {}: missing required field `version`",
                    manifest_path.display()
                );
                continue;
            }
        };
        let description = match req_str("description") {
            Some(v) => v,
            None => {
                eprintln!(
                    "[H8b] skipping {}: missing required field `description`",
                    manifest_path.display()
                );
                continue;
            }
        };
        let entrypoint = match req_str("entrypoint") {
            Some(v) => v,
            None => {
                eprintln!(
                    "[H8b] skipping {}: missing required field `entrypoint`",
                    manifest_path.display()
                );
                continue;
            }
        };
        let min_host_version = match req_str("minHostVersion") {
            Some(v) => v,
            None => {
                eprintln!(
                    "[H8b] skipping {}: missing required field `minHostVersion`",
                    manifest_path.display()
                );
                continue;
            }
        };

        // Optional fields.
        let backend_feature = req_str("backendFeature");

        let requires: Option<Vec<ModuleRequirement>> = partial
            .get("requires")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let dep_id = item.get("id")?.as_str()?.to_owned();
                        let dep_version = item
                            .get("version")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_owned());
                        Some(ModuleRequirement { id: dep_id, version: dep_version })
                    })
                    .collect()
            });

        // `permissions` is parsed leniently but FAIL-CLOSED: anything the shape doesn't fit
        // (a bare string, a number in `commands`, a missing `commands`) yields fewer
        // permissions, never more. A malformed declaration therefore produces a module that
        // cannot invoke, not one that can invoke anything.
        let string_list = |p: &serde_json::Value, key: &str| -> Vec<String> {
            p.get(key)
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|c| c.as_str())
                        .filter(|c| !c.is_empty())
                        .map(|c| c.to_owned())
                        .collect()
                })
                .unwrap_or_default()
        };
        let permissions = partial.get("permissions").map(|p| ModulePermissions {
            commands: string_list(p, "commands"),
            // Passed through as written; the host normalises and validates each entry (an
            // origin that is not `https://host[:port]` is dropped there, again fail-closed).
            origins: string_list(p, "origins"),
        });

        let module_dir_str = module_dir.to_string_lossy().into_owned();

        manifests.push(ExternalModuleManifest {
            id,
            name,
            version,
            description,
            entrypoint,
            backend_feature,
            requires,
            min_host_version,
            permissions,
            module_dir: module_dir_str,
        });
    }

    Ok(manifests)
}

/// Tauri command: scan the user's modules directory and return all valid manifests.
///
/// All filesystem I/O is moved onto a blocking thread via `spawn_blocking` so the async
/// executor is never blocked (AGENTS.md invariant: all disk-touching commands must be async).
#[tauri::command]
pub async fn list_external_modules() -> Result<Vec<ExternalModuleManifest>, String> {
    tauri::async_runtime::spawn_blocking(list_external_modules_blocking)
        .await
        .map_err(|e| e.to_string())?
}

/// Return the absolute path to the external-modules install directory
/// (`<app_data_dir>/modules/`). The directory is not created — it is shown in the
/// Modules panel as an install hint so the user knows where to drop module folders.
/// Returns an empty string if `app_data_dir` cannot be determined.
#[tauri::command]
pub fn get_modules_dir() -> String {
    match app_data_dir() {
        Ok(d) => d.join("modules").to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!("[H8d] could not determine app_data_dir for modules dir: {e}");
            String::new()
        }
    }
}

#[cfg(test)]
mod external_module_tests {
    use super::*;
    use super::test_env_helpers::EnvGuard;
    use std::path::Path;

    fn temp_xdg(tag: &str) -> crate::test_support::TestTmpDir {
        crate::test_support::TestTmpDir::new(&format!("extmod-{tag}"))
    }

    /// Write a manifest JSON file into `<xdg>/chairphoto/modules/<id>/chairphoto-module.json`.
    fn write_manifest(xdg: &Path, id: &str, json: &str) {
        let dir = xdg.join("chairphoto").join("modules").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("chairphoto-module.json"), json).unwrap();
    }

    // ── absent modules directory returns empty list ───────────────────────────

    #[test]
    fn absent_modules_dir_returns_empty() {
        let xdg = temp_xdg("absent");
        let _g = EnvGuard::set("XDG_DATA_HOME", xdg.to_str().unwrap());
        // No modules/ directory created — expect empty result, not an error.
        let result = list_external_modules_blocking().unwrap();
        assert!(result.is_empty(), "absent modules dir must return empty list");
    }

    // ── well-formed minimal manifest round-trips correctly ────────────────────

    #[test]
    fn valid_manifest_is_discovered() {
        let xdg = temp_xdg("valid");
        let _g = EnvGuard::set("XDG_DATA_HOME", xdg.to_str().unwrap());

        write_manifest(
            &xdg,
            "my-module",
            r#"{
                "id": "my-module",
                "name": "My Module",
                "version": "1.2.3",
                "description": "A test module.",
                "entrypoint": "index.js",
                "minHostVersion": "0.1.0"
            }"#,
        );

        let result = list_external_modules_blocking().unwrap();
        assert_eq!(result.len(), 1);
        let m = &result[0];
        assert_eq!(m.id, "my-module");
        assert_eq!(m.name, "My Module");
        assert_eq!(m.version, "1.2.3");
        assert_eq!(m.description, "A test module.");
        assert_eq!(m.entrypoint, "index.js");
        assert_eq!(m.min_host_version, "0.1.0");
        assert!(m.backend_feature.is_none());
        assert!(m.requires.is_none());
        // No `permissions` key at all: absent, which the host reads as "declares nothing"
        // and therefore refuses every api.invoke. Fail-closed by omission.
        assert!(m.permissions.is_none());
        // module_dir should end with the module id.
        assert!(
            m.module_dir.ends_with("my-module"),
            "module_dir should end with 'my-module', got: {}",
            m.module_dir
        );
    }

    // ── optional fields are parsed correctly ──────────────────────────────────

    #[test]
    fn optional_fields_are_parsed() {
        let xdg = temp_xdg("optional");
        let _g = EnvGuard::set("XDG_DATA_HOME", xdg.to_str().unwrap());

        write_manifest(
            &xdg,
            "rich-module",
            r#"{
                "id": "rich-module",
                "name": "Rich Module",
                "version": "0.5.0",
                "description": "Has optional fields.",
                "entrypoint": "dist/main.js",
                "backendFeature": "ai",
                "requires": [
                    { "id": "localsend", "version": "^0.1.0" },
                    { "id": "map" }
                ],
                "minHostVersion": "0.2.0",
                "permissions": { "commands": ["ai_suggest_tags", "ai_get_suggestions"] },
                "unknownFutureField": "ignored"
            }"#,
        );

        let result = list_external_modules_blocking().unwrap();
        assert_eq!(result.len(), 1);
        let m = &result[0];
        assert_eq!(m.backend_feature.as_deref(), Some("ai"));
        let reqs = m.requires.as_ref().expect("requires must be Some");
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].id, "localsend");
        assert_eq!(reqs[0].version.as_deref(), Some("^0.1.0"));
        assert_eq!(reqs[1].id, "map");
        assert!(reqs[1].version.is_none());
        let perms = m.permissions.as_ref().expect("permissions must be Some");
        assert_eq!(perms.commands, vec!["ai_suggest_tags", "ai_get_suggestions"]);
        // A pre-#49 manifest declares no origins at all, which is the shape every manifest
        // already on disk has. It must parse, and it must mean "reaches nothing".
        assert!(perms.origins.is_empty());
    }

    // ── `origins` is a sibling field, so old manifests keep parsing ────────────

    #[test]
    fn network_origins_are_parsed_beside_commands() {
        let xdg = temp_xdg("origins");
        let _g = EnvGuard::set("XDG_DATA_HOME", xdg.to_str().unwrap());

        write_manifest(
            &xdg,
            "net-module",
            r#"{
                "id": "net-module",
                "name": "Net Module",
                "version": "1.0.0",
                "description": "Declares network destinations.",
                "entrypoint": "index.js",
                "minHostVersion": "0.1.0",
                "permissions": {
                    "commands": ["get_photo"],
                    "origins": ["https://api.flickr.com", "https://up.flickr.com:8443", 7, ""]
                }
            }"#,
        );

        let result = list_external_modules_blocking().unwrap();
        let perms = result
            .iter()
            .find(|m| m.id == "net-module")
            .expect("net-module is discovered")
            .permissions
            .as_ref()
            .expect("permissions key present → Some");
        assert_eq!(perms.commands, vec!["get_photo"]);
        // Junk entries are dropped here exactly as they are in `commands`; whether each
        // surviving string is a usable origin is decided by the host, which normalises it.
        assert_eq!(
            perms.origins,
            vec!["https://api.flickr.com", "https://up.flickr.com:8443"]
        );
    }

    // ── a malformed `permissions` block narrows, never widens ─────────────────
    //
    // The point of the assertions below is direction: every shape the parser does not
    // understand must produce FEWER commands, never a wildcard or an "allow all". A
    // permission parser that fails open is worse than no parser.

    #[test]
    fn malformed_permissions_fail_closed() {
        let xdg = temp_xdg("perm-malformed");
        let _g = EnvGuard::set("XDG_DATA_HOME", xdg.to_str().unwrap());

        // `permissions` present but not an object → object accessors miss → no commands.
        write_manifest(
            &xdg,
            "perm-string",
            r#"{
                "id": "perm-string",
                "name": "Bare string",
                "version": "1.0.0",
                "description": "permissions is a string, not an object.",
                "entrypoint": "index.js",
                "minHostVersion": "0.1.0",
                "permissions": "all"
            }"#,
        );
        // `commands` present but not an array, plus non-string and empty entries elsewhere.
        write_manifest(
            &xdg,
            "perm-mixed",
            r#"{
                "id": "perm-mixed",
                "name": "Mixed entries",
                "version": "1.0.0",
                "description": "commands holds junk alongside one real command.",
                "entrypoint": "index.js",
                "minHostVersion": "0.1.0",
                "permissions": { "commands": ["get_photo", 7, null, "", { "x": 1 }] }
            }"#,
        );
        // An object with no `commands` key at all.
        write_manifest(
            &xdg,
            "perm-empty",
            r#"{
                "id": "perm-empty",
                "name": "Empty object",
                "version": "1.0.0",
                "description": "permissions object without commands.",
                "entrypoint": "index.js",
                "minHostVersion": "0.1.0",
                "permissions": {}
            }"#,
        );

        let result = list_external_modules_blocking().unwrap();
        let by_id = |id: &str| {
            result
                .iter()
                .find(|m| m.id == id)
                .unwrap_or_else(|| panic!("{id} should still be discovered"))
                .permissions
                .as_ref()
                .expect("permissions key present → Some")
                .commands
                .clone()
        };

        // None of the three is skipped — a bad permission block is not a bad manifest —
        // and none of them ends up with anything it did not spell out as a string.
        assert!(by_id("perm-string").is_empty());
        assert_eq!(by_id("perm-mixed"), vec!["get_photo"]);
        assert!(by_id("perm-empty").is_empty());
    }

    // ── malformed JSON is skipped; valid sibling is still returned ────────────

    #[test]
    fn malformed_manifest_is_skipped_others_returned() {
        let xdg = temp_xdg("malformed");
        let _g = EnvGuard::set("XDG_DATA_HOME", xdg.to_str().unwrap());

        // Bad JSON — truncated.
        write_manifest(&xdg, "bad-module", r#"{ "id": "bad", "#);

        // Good sibling.
        write_manifest(
            &xdg,
            "good-module",
            r#"{
                "id": "good-module",
                "name": "Good",
                "version": "1.0.0",
                "description": "Survives alongside the bad one.",
                "entrypoint": "index.js",
                "minHostVersion": "0.1.0"
            }"#,
        );

        let result = list_external_modules_blocking().unwrap();
        // Only the good module must appear.
        assert_eq!(result.len(), 1, "only one valid module expected");
        assert_eq!(result[0].id, "good-module");
    }

    // ── missing required field is treated as malformed ────────────────────────

    #[test]
    fn missing_required_field_is_skipped() {
        let xdg = temp_xdg("missing-field");
        let _g = EnvGuard::set("XDG_DATA_HOME", xdg.to_str().unwrap());

        // Missing `entrypoint`.
        write_manifest(
            &xdg,
            "incomplete-module",
            r#"{
                "id": "incomplete-module",
                "name": "Incomplete",
                "version": "1.0.0",
                "description": "Missing entrypoint field.",
                "minHostVersion": "0.1.0"
            }"#,
        );

        let result = list_external_modules_blocking().unwrap();
        assert!(result.is_empty(), "module with missing required field must be skipped");
    }

    // ── non-directory entries in modules/ are ignored ─────────────────────────

    #[test]
    fn non_directory_entries_are_ignored() {
        let xdg = temp_xdg("non-dir");
        let _g = EnvGuard::set("XDG_DATA_HOME", xdg.to_str().unwrap());

        // Create the modules dir and plant a plain file (not a dir) inside it.
        let modules_dir = xdg.join("chairphoto").join("modules");
        std::fs::create_dir_all(&modules_dir).unwrap();
        std::fs::write(modules_dir.join("not-a-module.txt"), "stray file").unwrap();

        let result = list_external_modules_blocking().unwrap();
        assert!(result.is_empty(), "non-directory entries must be ignored");
    }

    // ── multiple valid modules all returned ───────────────────────────────────

    #[test]
    fn multiple_valid_modules_all_returned() {
        let xdg = temp_xdg("multi");
        let _g = EnvGuard::set("XDG_DATA_HOME", xdg.to_str().unwrap());

        for id in ["mod-a", "mod-b", "mod-c"] {
            write_manifest(
                &xdg,
                id,
                &format!(
                    r#"{{
                        "id": "{id}",
                        "name": "{id}",
                        "version": "1.0.0",
                        "description": "Module {id}.",
                        "entrypoint": "index.js",
                        "minHostVersion": "0.1.0"
                    }}"#
                ),
            );
        }

        let result = list_external_modules_blocking().unwrap();
        assert_eq!(result.len(), 3, "all three valid modules must be discovered");
        let ids: Vec<&str> = result.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"mod-a"));
        assert!(ids.contains(&"mod-b"));
        assert!(ids.contains(&"mod-c"));
    }
}

